//! One review: the agent reads the pull request's checkout and answers with
//! schema-checked findings anchored to diff lines. Anchors that do not hold are
//! sent back once for re-anchoring; whatever still fails lands in the review body
//! instead of being dropped.

use crate::diff::{Diff, Side};
use crate::engine::{one_line, Engine, Outcome};
use crate::pr::{PrMeta, PrRef};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;

/// Diff text budget fed to the agent; beyond it whole files are deferred to a note.
pub const MAX_DIFF_BYTES: usize = 384 * 1024;
const MAX_DESCRIPTION_CHARS: usize = 4000;

#[derive(Debug, Clone, Deserialize)]
pub struct Finding {
    pub path: String,
    pub line: u64,
    #[serde(default)]
    pub start_line: Option<u64>,
    #[serde(default)]
    pub side: Option<String>,
    pub severity: String,
    #[serde(default)]
    pub title: Option<String>,
    pub body: String,
}

impl Finding {
    pub fn location(&self) -> String {
        match self.start_line {
            Some(start) if start != self.line => format!("{}:{}-{}", self.path, start.min(self.line), start.max(self.line)),
            _ => format!("{}:{}", self.path, self.line),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Answer {
    summary: String,
    verdict: String,
    findings: Vec<Finding>,
}

#[derive(Debug, Deserialize)]
struct Repairs {
    findings: Vec<Finding>,
}

/// A finding whose anchor the diff accepts, normalized for the API.
#[derive(Debug)]
pub struct Anchored {
    pub finding: Finding,
    pub side: Side,
    pub start: Option<u64>,
    pub line: u64,
}

pub struct Reviewed {
    pub summary: String,
    pub verdict: String,
    pub inline: Vec<Anchored>,
    /// Findings that stay in the review body, with why.
    pub fallback: Vec<(Finding, String)>,
    pub commands: u32,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

fn finding_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["path", "line", "severity", "body"],
        "properties": {
            "path": { "type": "string", "description": "repository-relative file path exactly as in the diff" },
            "line": { "type": "integer", "minimum": 1, "description": "line number shown in the diff; new-file numbering on RIGHT, old-file on LEFT" },
            "start_line": { "type": "integer", "minimum": 1, "description": "first line of a multi-line range, same hunk, below line" },
            "side": { "enum": ["RIGHT", "LEFT"], "description": "RIGHT (default): added or context line; LEFT: deleted code" },
            "severity": { "enum": ["blocker", "major", "minor", "nit"] },
            "title": { "type": "string", "description": "at most ~8 words" },
            "body": { "type": "string", "minLength": 1, "description": "the review comment to post, Markdown" }
        }
    })
}

fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "verdict", "findings"],
        "properties": {
            "summary": { "type": "string", "minLength": 1, "description": "a few sentences: what the change does and how sound it is" },
            "verdict": { "enum": ["approve", "comment", "request_changes"] },
            "findings": { "type": "array", "items": finding_schema() }
        }
    })
}

fn repair_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["findings"],
        "properties": { "findings": { "type": "array", "items": finding_schema() } }
    })
}

/// The single review prompt: PR identity, description, rules, then the diff.
pub fn prompt(r: &PrRef, meta: &PrMeta, sha: &str, diff_text: &str, skipped: &[String]) -> String {
    let mut p = format!(
        "Review GitHub pull request #{} of {}: {}\nAuthor {}; merging `{}` into `{}`; head commit {}.\n",
        r.number,
        r.slug(),
        meta.title,
        meta.author,
        meta.head_ref,
        meta.base_ref,
        &sha[..sha.len().min(12)],
    );
    let description: String = meta.body.trim().chars().take(MAX_DESCRIPTION_CHARS).collect();
    if !description.is_empty() {
        let cut = if meta.body.trim().chars().count() > MAX_DESCRIPTION_CHARS { "…" } else { "" };
        p.push_str(&format!("\n<description>\n{description}{cut}\n</description>\n"));
    }
    p.push_str(
        "\nYour working directory is exactly the head commit. Review the change below: find real defects it \
introduces or exposes — correctness, concurrency, data loss, security, performance regressions, resource \
leaks, API misuse, and docs or comments the change makes wrong. Read enough surrounding code to be sure; \
do not guess. Skip style opinions a formatter or linter would raise, and anything the change does not \
touch. No praise: every finding must be worth a reviewer's interruption.\n\
\n\
Findings become inline review comments; anchor them precisely:\n\
- `path`: the repository-relative file path exactly as the diff shows it.\n\
- `line` (and `start_line` for a range): 1-based numbers in the NEW (head) file, and each must be a line \
the diff shows as added (+) or context ( ). Only for a remark about deleted code set `side` to \"LEFT\" \
and use OLD-file numbers of removed (-) or context lines.\n\
- A range stays inside one diff hunk, `start_line` strictly below `line`.\n\
- Verify every number by reading the file; never estimate.\n\
- `severity`: blocker (must not merge) | major (should not merge as is) | minor (worth fixing) | nit \
(take or leave). `body`: the comment to post — specific, actionable Markdown with the reasoning a \
maintainer needs.\n\
Also return `summary` (what the change does, how sound it is) and `verdict`: approve, comment, or \
request_changes.\n",
    );
    if !skipped.is_empty() {
        p.push_str(&format!(
            "\nExcluded from this review (do not comment on them): {}\n",
            skipped.join(", ")
        ));
    }
    p.push_str(&format!("\n<diff>\n{diff_text}\n</diff>\n"));
    p
}

fn side_of(f: &Finding) -> Side {
    match f.side.as_deref() {
        Some("LEFT") => Side::Left,
        _ => Side::Right,
    }
}

/// Normalize (drop no-op ranges, swap reversed ones) and check against the diff.
fn validate(diff: &Diff, f: &Finding) -> Result<Anchored, (Finding, String)> {
    let side = side_of(f);
    let start = f.start_line.filter(|&s| s != f.line);
    let (start, line) = match start {
        Some(s) if s > f.line => (Some(f.line), s),
        other => (other, f.line),
    };
    match diff.check(&f.path, side, start, line) {
        Ok(()) => Ok(Anchored { finding: f.clone(), side, start, line }),
        Err(why) => Err((f.clone(), why)),
    }
}

fn repair_prompt(invalid: &[(Finding, String)]) -> String {
    let mut p = String::from("These findings could not be anchored to the pull request diff:\n");
    for (i, (f, why)) in invalid.iter().enumerate() {
        p.push_str(&format!("{}. {} ({}) — {}\n   {}\n", i + 1, f.location(), side_of(f).api(), why, one_line(&f.body, 200)));
    }
    p.push_str(
        "\nRe-anchor each one to a line the diff actually shows (new-file numbering on RIGHT: added or \
context lines; old-file numbering on LEFT: removed or context lines; ranges inside one hunk). Read the \
files again for exact numbers. Return the corrected findings in the same order, one per listed finding, \
keeping each body (adjust only if the new anchor requires it).\n",
    );
    p
}

pub async fn run(
    engine: &Engine,
    name: &str,
    prompt: &str,
    diff: &Diff,
    cancel: &Notify,
    mut progress: impl FnMut(String),
) -> Result<Reviewed, String> {
    let session = engine.start_session(name).await?;
    let out: Outcome<Answer> = engine.turn(&session, prompt, &schema(), cancel, &mut progress).await?;
    let (mut commands, mut input, mut cached, mut output) =
        (out.commands, out.input_tokens, out.cached_input_tokens, out.output_tokens);
    let answer = out.answer;
    let mut inline = Vec::new();
    let mut invalid = Vec::new();
    for f in &answer.findings {
        match validate(diff, f) {
            Ok(a) => inline.push(a),
            Err(bad) => invalid.push(bad),
        }
    }
    let mut fallback = Vec::new();
    if !invalid.is_empty() {
        progress(format!("re-anchoring {} finding(s)", invalid.len()));
        match engine.turn::<Repairs>(&session, &repair_prompt(&invalid), &repair_schema(), cancel, &mut progress).await {
            Ok(fix) => {
                commands += fix.commands;
                input += fix.input_tokens;
                cached += fix.cached_input_tokens;
                output += fix.output_tokens;
                let repaired = fix.answer.findings;
                for (i, (original, why)) in invalid.into_iter().enumerate() {
                    match repaired.get(i).map(|r| validate(diff, r)) {
                        Some(Ok(a)) => inline.push(a),
                        _ => fallback.push((original, why)),
                    }
                }
            }
            Err(e) => {
                progress(format!("re-anchoring failed: {e}"));
                fallback = invalid;
            }
        }
    }
    Ok(Reviewed {
        summary: answer.summary,
        verdict: answer.verdict,
        inline,
        fallback,
        commands,
        input_tokens: input,
        cached_input_tokens: cached,
        output_tokens: output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(path: &str, start: Option<u64>, line: u64, side: Option<&str>) -> Finding {
        Finding {
            path: path.into(),
            line,
            start_line: start,
            side: side.map(str::to_owned),
            severity: "minor".into(),
            title: None,
            body: "b".into(),
        }
    }

    #[test]
    fn validation_normalizes_ranges_and_reports_why_anchors_fail() {
        let diff = Diff::parse(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,3 @@\n context1\n+added2\n context3\n",
        );
        // Reversed range is swapped, not rejected.
        let a = validate(&diff, &finding("f.rs", Some(3), 1, None)).unwrap();
        assert_eq!((a.start, a.line), (Some(1), 3));
        // A degenerate range collapses to a single line.
        let a = validate(&diff, &finding("f.rs", Some(2), 2, None)).unwrap();
        assert_eq!((a.start, a.line), (None, 2));
        let (_, why) = validate(&diff, &finding("f.rs", None, 9, None)).unwrap_err();
        assert!(why.contains("not shown"));
        let (_, why) = validate(&diff, &finding("g.rs", None, 1, None)).unwrap_err();
        assert!(why.contains("not part of the reviewed diff"));
        // LEFT uses old numbering: old line 2 is context3.
        assert!(validate(&diff, &finding("f.rs", None, 2, Some("LEFT"))).is_ok());
    }
}
