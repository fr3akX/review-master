//! PR flows: the end-to-end paths through the code — a request, command, event or
//! job — whose behavior the change adds or alters, each as ordered steps over line
//! ranges at the PR head, so the reviewer can walk the change in execution order
//! instead of file order. Like cxplr's code walkthroughs, scoped to one change:
//! every step is checked against the files, and steps on changed lines are marked.

use crate::diff::{Kind, Model};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Part of the cache key: bump when the prompt or schema changes.
pub const VERSION: u32 = 1;
pub const KINDS: [&str; 7] = ["entry", "call", "branch", "transform", "io", "return", "error"];
/// Longest range one step may highlight.
const MAX_SPAN: i64 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub path: String,
    pub line: i64,
    pub end_line: i64,
    pub kind: String,
    pub title: String,
    /// What the code does here in this flow.
    pub detail: String,
    /// What the change does at this step; empty for unchanged context.
    pub change: String,
    /// Set by the server: the range touches lines the diff adds or removes.
    #[serde(default)]
    pub changed: bool,
    /// Set by the server: the file exists and the range is inside it.
    #[serde(default)]
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flow {
    pub name: String,
    pub description: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Flows {
    pub flows: Vec<Flow>,
}

pub fn schema() -> Value {
    let step = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["path", "line", "end_line", "kind", "title", "detail", "change"],
        "properties": {
            "path": { "type": "string" },
            "line": { "type": "integer" },
            "end_line": { "type": "integer" },
            "kind": { "type": "string", "enum": KINDS },
            "title": { "type": "string" },
            "detail": { "type": "string" },
            "change": { "type": "string" }
        }
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["flows"],
        "properties": {
            "flows": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "description", "steps"],
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "steps": { "type": "array", "items": step }
                    }
                }
            }
        }
    })
}

/// `context` names the change (subject, description); `diff` is the prompt's diff block.
pub fn prompt(context: &str, diff: &str) -> String {
    let mut p = String::from(context);
    p.push_str(
        "
Map the flows this change affects, so the reviewer can read it in execution order instead of file \
order. A flow is an end-to-end path through the code — a request, command, event, job or data path — \
whose behavior the change adds or alters. The repository at the PR head is the current working \
directory; investigate read-only with the repository tools. Do not modify files or run the project's \
code, builds or tests.

Instructions:
- flows: 1-4, most important first; one per distinct entry point or behavior, not one per file. \
Changes that sit on no runtime path (docs, configuration, pure refactors) need no flow; return an \
empty list when nothing in the change runs.
- name: at most 60 characters, e.g. \"Cache read with expiry\". description: at most 300 characters: \
what triggers the flow, what it produces, and what is different after this change.
- steps: 3-25 locations in the order the flow executes them, from its entry point to its outcome. \
Include the changed code on the path, and only as much unchanged code as needed to connect it.
- One step per meaningful point: entry (where the input arrives), call (into another function or \
file), branch (a condition: which way it goes and why), transform (data is validated, parsed, \
mapped or computed), io (database, file, network, queue or another service), return (a result goes \
back), error (a failure or rejection).
- path and line: repository-relative file and 1-based first line at the PR head; end_line: its last \
line (at most 40 lines later). Take line numbers from tool output that shows them (numbered reads, \
`rg -n`) or from the new-file numbers of the diff hunks; never estimate them.
- title: what happens, at most 80 characters. detail: what the code does here in this flow, at most \
400 characters. change: what this pull request changes at this step compared to before, at most 300 \
characters; empty when the step is unchanged context.

Return ONLY the JSON object required by the output schema.
",
    );
    p.push_str(diff);
    p
}

/// Head-file lines the diff touches, per file: added lines, and the head line after which
/// deleted lines were (0 = before the first line).
fn touched(model: &Model) -> HashMap<&str, (HashSet<u64>, HashSet<u64>)> {
    let mut out = HashMap::new();
    for file in &model.files {
        let (added, deleted_after): &mut (HashSet<u64>, HashSet<u64>) = out.entry(file.path.as_str()).or_default();
        for hunk in &file.hunks {
            let mut last_new = hunk.lines.iter().find_map(|l| l.new).unwrap_or(1).saturating_sub(1);
            for l in &hunk.lines {
                match l.kind {
                    Kind::Add => {
                        added.insert(l.new.unwrap_or(0));
                    }
                    Kind::Del => {
                        deleted_after.insert(last_new);
                    }
                    Kind::Ctx => {}
                }
                if let Some(n) = l.new {
                    last_new = n;
                }
            }
        }
    }
    out
}

/// Check every step against the files: unknown files or lines get a problem, ranges are
/// clamped to the file and to a readable span, and steps on changed lines are marked.
pub fn verify(root: &Path, model: &Model, flows: &mut Flows) {
    let touched = touched(model);
    let mut files: HashMap<String, Result<(String, usize), String>> = HashMap::new();
    flows.flows.retain(|f| !f.steps.is_empty());
    for step in flows.flows.iter_mut().flat_map(|f| f.steps.iter_mut()) {
        step.verified = false;
        step.changed = false;
        step.problem = None;
        if !KINDS.contains(&step.kind.as_str()) {
            step.kind = "transform".into();
        }
        let found = files.entry(step.path.clone()).or_insert_with(|| crate::verify::file_lines(root, &step.path));
        let (shown, total) = match found {
            Ok((shown, total)) => (shown.clone(), *total as i64),
            Err(problem) => {
                step.problem = Some(problem.clone());
                continue;
            }
        };
        step.path = shown;
        if step.line < 1 {
            step.problem = Some("line numbers start at 1".into());
            continue;
        }
        if step.line > total {
            step.problem = Some(format!("file has only {total} lines"));
            continue;
        }
        step.end_line = step.end_line.clamp(step.line, total.min(step.line + MAX_SPAN));
        step.verified = true;
        if let Some((added, deleted_after)) = touched.get(step.path.as_str()) {
            let (from, to) = (step.line as u64, step.end_line as u64);
            step.changed = (from..=to).any(|n| added.contains(&n)) || (from - 1..=to).any(|n| deleted_after.contains(&n));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn step(path: &str, line: i64, end_line: i64, kind: &str) -> Step {
        Step {
            path: path.into(),
            line,
            end_line,
            kind: kind.into(),
            title: String::new(),
            detail: String::new(),
            change: String::new(),
            changed: false,
            verified: false,
            problem: None,
        }
    }

    #[test]
    fn steps_are_checked_clamped_and_marked_changed() {
        let dir = std::env::temp_dir().join(format!("review-master-flows-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), (1..=100).map(|n| format!("line {n}\n")).collect::<String>()).unwrap();
        let root = dir.canonicalize().unwrap();
        // Line 10 is added; one old line is removed after head line 50.
        let model = Model::parse(
            "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -9,2 +9,3 @@\n line 9\n+line 10\n line 11\n\
@@ -49,3 +50,2 @@\n line 50\n-gone\n line 51\n",
        );
        let mut flows = Flows {
            flows: vec![
                Flow {
                    name: "f".into(),
                    description: String::new(),
                    steps: vec![
                        step("./src/a.rs", 8, 12, "entry"),
                        step("src/a.rs", 20, 25, "call"),
                        step("src/a.rs", 51, 52, "branch"),
                        step("src/a.rs", 95, 140, "return"),
                        step("src/a.rs", 30, 5, "unknown-kind"),
                        step("src/a.rs", 1, 500, "io"),
                        step("src/a.rs", 101, 101, "call"),
                        step("src/missing.rs", 1, 2, "call"),
                        step("../outside.rs", 1, 2, "call"),
                    ],
                },
                Flow { name: "empty".into(), description: String::new(), steps: vec![] },
            ],
        };
        verify(&root, &model, &mut flows);
        assert_eq!(flows.flows.len(), 1, "flows without steps are dropped");
        let got: Vec<(bool, bool, i64, &str)> =
            flows.flows[0].steps.iter().map(|s| (s.verified, s.changed, s.end_line, s.kind.as_str())).collect();
        assert_eq!(
            got,
            [
                (true, true, 12, "entry"),
                (true, false, 25, "call"),
                (true, true, 52, "branch"),     // the deletion sits right before line 51
                (true, false, 100, "return"),   // clamped to the file
                (true, false, 30, "transform"), // an end before the start shows one line; unknown kinds fall back
                (true, true, 61, "io"),         // spans are capped
                (false, false, 101, "call"),
                (false, false, 2, "call"),
                (false, false, 2, "call"),
            ]
        );
        assert_eq!(flows.flows[0].steps[0].path, "src/a.rs");
        assert_eq!(flows.flows[0].steps[6].problem.as_deref(), Some("file has only 100 lines"));
        assert_eq!(flows.flows[0].steps[8].problem.as_deref(), Some("outside the repository"));
        let _ = fs::remove_dir_all(&dir);
    }
}
