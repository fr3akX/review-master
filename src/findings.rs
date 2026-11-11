//! Findings from another LLM's review, imported as Markdown. Reviews come in many
//! shapes — a heading per finding, bullet lists under severity headings, "Location:"
//! fields — so the text is split heuristically: a heading section that cites a
//! `path:line` is one finding, unless it lists two or more located bullets, which are
//! one finding each. Every cited path is placed in the checkout (diff prefixes, URLs,
//! bare file names of changed files) so the UI can annotate exactly those lines and
//! turn the findings the reviewer agrees with into draft comments.

use crate::diff::{Model, Status};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Severities, mapped onto the review's own scale.
const SEVERITY_WORDS: &[(&str, &str)] = &[
    ("blocker", "blocker"),
    ("blocking", "blocker"),
    ("critical", "blocker"),
    ("severe", "blocker"),
    ("p0", "blocker"),
    ("major", "major"),
    ("high", "major"),
    ("important", "major"),
    ("p1", "major"),
    ("minor", "minor"),
    ("medium", "minor"),
    ("moderate", "minor"),
    ("warning", "minor"),
    ("p2", "minor"),
    ("nit", "nit"),
    ("nitpick", "nit"),
    ("low", "nit"),
    ("trivial", "nit"),
    ("style", "nit"),
    ("suggestion", "nit"),
    ("info", "nit"),
    ("optional", "nit"),
    ("cosmetic", "nit"),
    ("p3", "nit"),
];
const SEVERITY_EMOJI: &[(&str, &str)] = &[("🔴", "blocker"), ("🟠", "major"), ("🟡", "minor"), ("🔵", "nit"), ("⚪", "nit"), ("🟢", "nit"), ("💡", "nit")];
const DEFAULT_SEVERITY: &str = "minor";
/// Sections about the change as a whole; citations in them are context, not findings.
const OVERVIEW: &[&str] = &[
    "summary", "overview", "verdict", "conclusion", "strengths", "positives", "praise", "what's good", "what is good", "tl;dr", "tldr",
    "recommendation", "context", "background", "approval", "overall", "review", "code review", "pr review", "pull request",
];
const MAX_BODY_CHARS: usize = 8000;

/// `path:12`, `path:12-18`, `path#L12-L18`, `path:12:5`, `path (lines 12-18)`, `path line 12`: a
/// path with a directory or an extension, then a line or a range.
static CITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?P<path>(?:[A-Za-z0-9_.\-]+/)*[A-Za-z0-9_\-][A-Za-z0-9_.\-]*\.[A-Za-z][A-Za-z0-9]*|(?:[A-Za-z0-9_.\-]+/)+[A-Za-z0-9_.\-]+)",
        r"[`*]*",
        r"(?:(?::|#)L?(?P<a>\d+)(?::\d+)?(?:\s*(?:-|–|—|\.\.|to)\s*L?(?P<b>\d+))?",
        r"|\s*[,(]?\s*(?:lines?|L)\s*(?P<c>\d+)(?:\s*(?:-|–|—|\.\.|to)\s*(?P<d>\d+))?\)?)",
    ))
    .unwrap()
});
static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$").unwrap());
static ITEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ ?(?:[-*+]|\d{1,3}[.)])\s+").unwrap());
/// A bullet that is a field of one finding (`**Location:** …`), not a finding of its own.
static LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[*_`]*[A-Za-z][A-Za-z ]{0,24}[*_`]*:(?:\s|\*|$)").unwrap());
static SEVERITY_FIELD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:severity|priority)\W{0,6}([a-z0-9]+)").unwrap());
static SEVERITY_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\[(]\s*([A-Za-z0-9]+)\s*[\])]").unwrap());
static SEVERITY_LEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^A-Za-z0-9]*([A-Za-z0-9]+)(?:[*_\])]*\s*:|\s+[-–—|]\s|[*_\])]*\s*$)").unwrap());
static STRONG_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(blocker|blocking|critical|major|minor|nits?|nitpicks?)\b").unwrap());
static TITLE_NOISE: LazyLock<Regex> = LazyLock::new(|| {
    let words = SEVERITY_WORDS.iter().map(|(w, _)| *w).collect::<Vec<_>>().join("|");
    Regex::new(&format!(
        r"(?i)[\[(]\s*(?:{words})s?\s*[\])]|\b(?:severity|priority)\W{{0,6}}[a-z0-9]+|^[^A-Za-z0-9]*(?:{words})s?(?:[*_\])]*\s*:|\s+[-–—|]\s|[*_\])]*\s*$)|\*\*|__|``|🔴|🟠|🟡|🔵|⚪|🟢|💡|⚠️?"
    ))
    .unwrap()
});
static NUMBERING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:\d{1,3}[.)]\s+|#\d+[.:]?\s*|(?:finding|issue)\s*#?\d+\s*[:.\-–—]?\s*|(?:finding|issue|title)\s*:\s*)").unwrap());
static TRAILING_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s+(?:in|at|on|see)$").unwrap());

/// One imported finding, placed in the checkout.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finding {
    pub id: u64,
    /// Name of the imported document.
    pub source: String,
    pub path: String,
    /// Last line of the cited range, 1-based (drafts anchor on it).
    pub line: u64,
    #[serde(default)]
    pub start: Option<u64>,
    /// `RIGHT` (head) or `LEFT` (a file the change deletes).
    pub side: String,
    pub severity: String,
    pub title: String,
    pub body: String,
    /// The reference as written, e.g. `cache.rs:16`.
    pub cited: String,
    /// `open`, `dismissed` or `drafted`.
    pub status: String,
    /// Why the reference could not be placed (unknown file, line past the end).
    #[serde(default)]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Cite {
    path: String,
    start: u64,
    end: u64,
    raw: String,
}

/// A finding as written, before it is placed.
#[derive(Debug)]
struct Parsed {
    title: String,
    body: String,
    severity: &'static str,
    cites: Vec<Cite>,
}

fn cites(text: &str) -> Vec<Cite> {
    CITE.captures_iter(text)
        .filter_map(|c| {
            let start: u64 = c.name("a").or_else(|| c.name("c"))?.as_str().parse().ok()?;
            let end = c.name("b").or_else(|| c.name("d")).and_then(|m| m.as_str().parse::<u64>().ok()).filter(|&e| e >= start).unwrap_or(start);
            (start > 0).then(|| Cite { path: c["path"].to_owned(), start, end, raw: c[0].trim_end_matches(')').to_owned() })
        })
        .collect()
}

/// A severity word, singular or plural (`Nits`, `Suggestions`).
fn severity_word(word: &str) -> Option<&'static str> {
    let word = word.to_ascii_lowercase();
    let singular = word.strip_suffix('s').unwrap_or(&word);
    SEVERITY_WORDS.iter().find(|(w, _)| *w == word || *w == singular).map(|(_, s)| *s)
}

/// Explicit severity markers: `Severity: high`, `[major]`, `(nit)`, a leading `Critical:`, emoji.
fn marked_severity(text: &str) -> Option<&'static str> {
    SEVERITY_FIELD
        .captures_iter(text)
        .chain(SEVERITY_TAG.captures_iter(text))
        .find_map(|c| severity_word(&c[1]))
        .or_else(|| SEVERITY_LEAD.captures(text.trim()).and_then(|c| severity_word(&c[1])))
        .or_else(|| SEVERITY_EMOJI.iter().find(|(e, _)| text.contains(e)).map(|(_, s)| *s))
}

/// A severity in a heading or bullet title: marked, or one of the unambiguous words.
fn title_severity(text: &str) -> Option<&'static str> {
    marked_severity(text).or_else(|| STRONG_WORD.captures(text).and_then(|c| severity_word(&c[1])))
}

fn clean_title(raw: &str) -> String {
    let mut s = CITE.replace_all(raw, "").into_owned();
    s = NUMBERING.replace(s.trim(), "").into_owned();
    s = TITLE_NOISE.replace_all(&s, "").into_owned();
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let trim: &[char] = &[' ', '-', '–', '—', ':', '|', ',', '.', '(', ')', '[', ']', '*', '_'];
    let mut s = s.trim_matches(trim).to_owned();
    while let Some(m) = TRAILING_WORD.find(&s) {
        s.truncate(m.start());
        s = s.trim_end_matches(trim).to_owned();
    }
    s
}

/// First sentence, at most `max` characters.
fn first_sentence(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let end = line.find(". ").map_or(line.len(), |i| i + 1);
    let mut s: String = line[..end].chars().take(max).collect();
    if s.chars().count() < line[..end].chars().count() {
        s.push('…');
    }
    s
}

fn is_overview(title: &str) -> bool {
    let t = title.to_lowercase();
    OVERVIEW.iter().any(|w| t.starts_with(w))
}

fn fence_marker(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

struct Section {
    level: usize,
    heading: String,
    lines: Vec<String>,
}

/// Heading sections; level 0 is the text before the first heading.
fn sections(text: &str) -> Vec<Section> {
    let mut out = vec![Section { level: 0, heading: String::new(), lines: Vec::new() }];
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        let last = out.last_mut().expect("never empty");
        if let Some(f) = fence {
            if line.trim_start().starts_with(f) && line.trim_start().trim_start_matches(f.chars().next().unwrap()).trim().is_empty() {
                fence = None;
            }
            last.lines.push(line.to_owned());
            continue;
        }
        if let Some(f) = fence_marker(line) {
            fence = Some(f);
            last.lines.push(line.to_owned());
            continue;
        }
        if let Some(c) = HEADING.captures(line) {
            out.push(Section { level: c[1].len(), heading: c[2].to_owned(), lines: Vec::new() });
            continue;
        }
        last.lines.push(line.to_owned());
    }
    out
}

struct Chunk {
    item: bool,
    lines: Vec<String>,
}

/// Top-level bullets (with their continuation lines) and the prose between them.
fn chunks(lines: &[String]) -> Vec<Chunk> {
    let mut out: Vec<Chunk> = Vec::new();
    let mut fence: Option<&str> = None;
    let mut prev_blank = true;
    for line in lines {
        if let Some(f) = fence {
            if line.trim_start().starts_with(f) {
                fence = None;
            }
            out.last_mut().expect("a fence opened in a chunk").lines.push(line.clone());
            continue;
        }
        let blank = line.trim().is_empty();
        let indented = line.starts_with("  ") || line.starts_with('\t');
        if ITEM.is_match(line) {
            out.push(Chunk { item: true, lines: vec![line.clone()] });
        } else {
            match out.last_mut() {
                Some(c) if !c.item || blank || indented || !prev_blank => c.lines.push(line.clone()),
                _ => out.push(Chunk { item: false, lines: vec![line.clone()] }),
            }
        }
        if fence_marker(line).is_some() {
            fence = fence_marker(line);
        }
        prev_blank = blank;
    }
    out
}

/// Lines joined with their common indentation removed and blank edges trimmed.
fn dedent(lines: &[String]) -> String {
    let indent = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    let body: Vec<&str> = lines.iter().map(|l| l.get(indent..).unwrap_or(l.trim_start())).collect();
    body.join("\n").trim_matches('\n').trim_end().to_owned()
}

fn build(title_src: &str, mut body: String, context: Option<&'static str>) -> Parsed {
    let mut all = cites(title_src);
    all.extend(cites(&body));
    let opening: String = body.lines().take(3).collect::<Vec<_>>().join("\n");
    let severity = title_severity(title_src).or_else(|| marked_severity(&opening)).or(context).unwrap_or(DEFAULT_SEVERITY);
    let mut title = clean_title(title_src);
    if title.is_empty() && !body.is_empty() {
        title = clean_title(&first_sentence(&body, 100));
    }
    if body.is_empty() {
        body = title_src.trim().to_owned();
    }
    if title.chars().count() > 120 {
        title = first_sentence(&title, 100);
    }
    if body.chars().count() > MAX_BODY_CHARS {
        body = body.chars().take(MAX_BODY_CHARS).collect::<String>() + "…";
    }
    Parsed { title, body, severity, cites: all }
}

/// Split a review into findings; also returns the headings of sections left out.
fn parse(text: &str) -> (Vec<Parsed>, Vec<String>) {
    let text = text.replace("\r\n", "\n");
    let mut found = Vec::new();
    let mut skipped = Vec::new();
    // Severity set by a heading without citations ("## Major issues"), with its level.
    let mut context: Vec<(usize, &'static str)> = Vec::new();
    for sec in sections(&text) {
        context.retain(|&(level, _)| level < sec.level || sec.level == 0);
        let inherited = context.last().map(|&(_, s)| s);
        let parts = chunks(&sec.lines);
        let located_items: Vec<&Chunk> = parts.iter().filter(|c| c.item && !cites(&c.lines.join("\n")).is_empty()).collect();
        let fields = located_items.iter().filter(|c| LABEL.is_match(ITEM.replace(&c.lines[0], "").trim_start())).count();
        let heading_cited = !cites(&sec.heading).is_empty();
        let content = dedent(&sec.lines);
        let content_cited = !cites(&content).is_empty();
        let section_severity = title_severity(&sec.heading).or(inherited);
        if !heading_cited && located_items.len() >= 2 && fields * 2 < located_items.len() {
            for c in located_items {
                let first = ITEM.replace(&c.lines[0], "").into_owned();
                found.push(build(&first, dedent(&c.lines[1..]), section_severity));
            }
        } else if heading_cited || (content_cited && !is_overview(&clean_title(&sec.heading))) {
            if sec.heading.trim().is_empty() {
                let mut lines = content.lines();
                let first = lines.next().unwrap_or("").to_owned();
                let first = ITEM.replace(&first, "").into_owned();
                found.push(build(&first, dedent(&lines.map(str::to_owned).collect::<Vec<_>>()), inherited));
            } else {
                found.push(build(&sec.heading, content, inherited));
            }
        } else if let Some(sev) = title_severity(&sec.heading).filter(|_| !content_cited) {
            context.push((sec.level, sev));
        } else if !sec.heading.trim().is_empty() && !content.trim().is_empty() {
            skipped.push(clean_title(&sec.heading));
        }
    }
    (found, skipped)
}

/// A cited path placed in the checkout: (repository path, its line count if it exists at head, deleted).
type Placed = (String, Option<usize>, bool);

/// Place a cited path: as written, without `./` or a diff prefix, as a suffix of a longer
/// path (URLs, absolute paths of another checkout), or a changed file it uniquely names.
fn place(root: &Path, model: &Model, raw: &str) -> Result<Placed, String> {
    let root_text = root.to_string_lossy();
    let cleaned = raw.trim_start_matches("./");
    let cleaned = cleaned.strip_prefix(&format!("{root_text}/")).unwrap_or(cleaned);
    let parts: Vec<&str> = cleaned.split('/').filter(|p| !p.is_empty()).collect();
    let in_model = |p: &str| model.files.iter().find(|f| f.path == p);
    for i in 0..parts.len() {
        let candidate = parts[i..].join("/");
        if let Some(f) = in_model(&candidate) {
            let lines = crate::verify::file_lines(root, &candidate).ok().map(|(_, n)| n);
            return Ok((f.path.clone(), lines, f.status == Status::Deleted));
        }
        if let Ok((shown, lines)) = crate::verify::file_lines(root, &candidate) {
            return Ok((shown, Some(lines), false));
        }
    }
    let unique = |hits: Vec<&crate::diff::FileEntry>| match hits.as_slice() {
        [f] => Some((f.path.clone(), f.status == Status::Deleted)),
        _ => None,
    };
    let joined = parts.join("/");
    let by_suffix = unique(model.files.iter().filter(|f| f.path.ends_with(&format!("/{joined}"))).collect());
    let by_name = || unique(model.files.iter().filter(|f| f.path.rsplit('/').next() == parts.last().copied()).collect());
    match by_suffix.or_else(by_name) {
        Some((path, deleted)) => {
            let lines = crate::verify::file_lines(root, &path).ok().map(|(_, n)| n);
            Ok((path, lines, deleted))
        }
        None => Err(format!("`{raw}` is not a file of the checkout")),
    }
}

fn locate(root: &Path, model: &Model, p: Parsed, source: &str) -> Finding {
    let placed: Vec<(Cite, Result<Placed, String>)> = p.cites.iter().map(|c| (c.clone(), place(root, model, &c.path))).collect();
    let (cite, found) = placed.iter().find(|(_, r)| r.is_ok()).or_else(|| placed.first()).cloned().expect("findings cite a location");
    let (path, problem, deleted) = match found {
        Ok((path, lines, deleted)) => {
            let past = lines.filter(|&n| !deleted && cite.end > n as u64);
            (path, past.map(|n| format!("line {} is past the end of the file ({n} lines)", cite.end)), deleted)
        }
        Err(e) => (cite.path.clone(), Some(e), false),
    };
    Finding {
        id: 0,
        source: source.to_owned(),
        path,
        line: cite.end,
        start: (cite.start != cite.end).then_some(cite.start),
        side: if deleted { "LEFT" } else { "RIGHT" }.to_owned(),
        severity: p.severity.to_owned(),
        title: p.title,
        body: p.body,
        cited: cite.raw,
        status: "open".to_owned(),
        problem,
    }
}

pub struct Imported {
    pub added: usize,
    pub skipped: Vec<String>,
}

/// Imported findings of this review, persisted under the cache like drafts.
#[derive(Default, Serialize, Deserialize)]
pub struct Store {
    next: u64,
    pub items: Vec<Finding>,
    #[serde(skip)]
    file: PathBuf,
}

impl Store {
    pub fn load(file: PathBuf) -> Store {
        let mut store: Store = fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        store.file = file;
        store
    }

    pub fn save(&self) {
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = fs::write(&self.file, bytes);
        }
    }

    /// Replace the findings of `source` with those parsed from `text`. A finding that is
    /// still there (same place and title, under any source name) keeps its status, so
    /// importing an updated review does not bring back what was dismissed.
    pub fn import(&mut self, source: &str, text: &str, root: &Path, model: &Model) -> Imported {
        let (parsed, skipped) = parse(text);
        let old = self.items.clone();
        self.items.retain(|f| f.source != source);
        let added = parsed.len();
        for p in parsed {
            let mut f = locate(root, model, p, source);
            if let Some(prev) = old.iter().find(|o| o.path == f.path && o.line == f.line && o.title == f.title) {
                f.status = prev.status.clone();
            }
            self.next += 1;
            f.id = self.next;
            self.items.push(f);
        }
        Imported { added, skipped }
    }

    pub fn set_status(&mut self, id: u64, status: &str) -> Result<(), String> {
        if !["open", "dismissed", "drafted"].contains(&status) {
            return Err(format!("unknown status `{status}`"));
        }
        let f = self.items.iter_mut().find(|f| f.id == id).ok_or_else(|| format!("no finding {id}"))?;
        f.status = status.to_owned();
        Ok(())
    }

    /// Drop every source but these.
    pub fn keep(&mut self, sources: &[&str]) {
        self.items.retain(|f| sources.contains(&f.source.as_str()));
    }

    /// Drop one source's findings, or all of them.
    pub fn remove(&mut self, source: Option<&str>) {
        self.items.retain(|f| source.is_some_and(|s| f.source != s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Vec<(String, &'static str, String)> {
        let (found, _) = parse(text);
        found.into_iter().map(|p| (p.title, p.severity, p.cites.first().map(|c| format!("{}:{}-{}", c.path, c.start, c.end)).unwrap_or_default())).collect()
    }

    #[test]
    fn citations_in_their_usual_spellings() {
        let got: Vec<String> = cites(
            "see `src/a.rs:12`, src/b.rs:3-9, c.py#L4-L6, https://github.com/o/r/blob/x/src/d.go#L7, `e.ts` (lines 10-12), \
             f.rs line 5, g.rs:8:3, version 1.2:3, e.g.: 5, h.rs:9 to 2",
        )
        .iter()
        .map(|c| format!("{}:{}-{}", c.path, c.start, c.end))
        .collect();
        assert_eq!(
            got,
            [
                "src/a.rs:12-12",
                "src/b.rs:3-9",
                "c.py:4-6",
                "github.com/o/r/blob/x/src/d.go:7-7",
                "e.ts:10-12",
                "f.rs:5-5",
                "g.rs:8-8",
                "h.rs:9-9", // a range that runs backwards is one line
            ]
        );
    }

    #[test]
    fn a_heading_per_finding() {
        let text = "# Review of #12\n\nOverall fine, touches src/a.rs:1.\n\n\
## 1. [major] Race in `Cache::get` (src/store/cache.rs:16-18)\n\nTwo readers can both miss.\n\n```rust\nlet x = 1;\n# not a heading\n```\n\n\
### Nit: naming — `ttl.rs:3`\nCall it `expires_at`.\n\n\
## Finding 3: Unchecked index\n**Location:** `src/api/handler.py:9`\n**Severity:** High\n\nIndexing can panic.\n";
        assert_eq!(
            parsed(text),
            [
                ("Race in `Cache::get`".to_owned(), "major", "src/store/cache.rs:16-18".to_owned()),
                ("naming".to_owned(), "nit", "ttl.rs:3-3".to_owned()),
                ("Unchecked index".to_owned(), "major", "src/api/handler.py:9-9".to_owned()),
            ]
        );
        let (found, skipped) = parse(text);
        assert!(found[0].body.contains("# not a heading"), "fenced lines stay in the body");
        assert_eq!(skipped, ["Review of #12"], "the overview section is not a finding");
    }

    #[test]
    fn bullets_under_severity_headings() {
        let text = "## 🔴 Critical\n- `src/a.rs:10` — Panics on empty input.\n  Guard it.\n- src/b.rs:4-6: Leaks the handle\n\n\
## Suggestions\n1. **c.rs:2** Prefer `matches!`\n2. d.rs:7 — Rename `x`\n\n## Tests\nNo tests cover the new path.\n";
        assert_eq!(
            parsed(text),
            [
                ("Panics on empty input".to_owned(), "blocker", "src/a.rs:10-10".to_owned()),
                ("Leaks the handle".to_owned(), "blocker", "src/b.rs:4-6".to_owned()),
                ("Prefer `matches!`".to_owned(), "nit", "c.rs:2-2".to_owned()),
                ("Rename `x`".to_owned(), "nit", "d.rs:7-7".to_owned()),
            ]
        );
        let (found, skipped) = parse(text);
        assert_eq!(found[0].body, "Guard it.");
        assert_eq!(found[1].body, "src/b.rs:4-6: Leaks the handle", "a one-line bullet is its own body");
        assert_eq!(skipped, ["Tests"]);
    }

    #[test]
    fn severity_context_ends_with_its_section() {
        let text = "## Major\n### Slow path\nsrc/a.rs:3 allocates per call.\n## Other\n### Typo\nsrc/b.rs:1 says teh.\n";
        let got: Vec<&str> = parsed(text).into_iter().map(|(_, s, _)| s).collect();
        assert_eq!(got, ["major", "minor"]);
    }

    #[test]
    fn findings_are_placed_in_the_checkout() {
        let dir = std::env::temp_dir().join(format!("review-master-findings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src/store")).unwrap();
        fs::write(dir.join("src/store/cache.rs"), "a\nb\nc\n").unwrap();
        fs::write(dir.join("README.md"), "x\n").unwrap();
        let root = dir.canonicalize().unwrap();
        let model = Model::parse(
            "diff --git a/src/store/cache.rs b/src/store/cache.rs\n--- a/src/store/cache.rs\n+++ b/src/store/cache.rs\n@@ -1,2 +1,3 @@\n a\n+b\n c\n\
diff --git a/src/old.rs b/src/old.rs\ndeleted file mode 100644\n--- a/src/old.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-gone\n",
        );
        let md = "- b/src/store/cache.rs:2 one\n- cache.rs:1-3 two\n- https://github.com/o/r/blob/abc/README.md#L1 three\n\
- src/store/cache.rs:9 four\n- nowhere.rs:1 five\n- old.rs:1 six\n";
        let mut store = Store::default();
        let imported = store.import("review.md", md, &root, &model);
        assert_eq!(imported.added, 6);
        let got: Vec<(&str, u64, Option<u64>, &str, bool)> =
            store.items.iter().map(|f| (f.path.as_str(), f.line, f.start, f.side.as_str(), f.problem.is_some())).collect();
        assert_eq!(
            got,
            [
                ("src/store/cache.rs", 2, None, "RIGHT", false),
                ("src/store/cache.rs", 3, Some(1), "RIGHT", false),
                ("README.md", 1, None, "RIGHT", false),
                ("src/store/cache.rs", 9, None, "RIGHT", true),
                ("nowhere.rs", 1, None, "RIGHT", true),
                ("src/old.rs", 1, None, "LEFT", false),
            ]
        );
        // Re-importing keeps what was dismissed; another source is independent.
        let id = store.items[0].id;
        store.set_status(id, "dismissed").unwrap();
        store.import("other.md", "- README.md:1 seven\n- README.md:1 eight\n", &root, &model);
        store.import("review.md", md, &root, &model);
        let statuses: Vec<(&str, &str)> = store.items.iter().map(|f| (f.source.as_str(), f.status.as_str())).take(3).collect();
        assert_eq!(statuses, [("other.md", "open"), ("other.md", "open"), ("review.md", "dismissed")]);
        // A re-run under another name: what was dismissed stays dismissed, and `keep` drops the earlier run.
        store.import("rerun.md", md, &root, &model);
        assert_eq!(store.items.iter().find(|f| f.source == "rerun.md").unwrap().status, "dismissed");
        store.keep(&["rerun.md"]);
        assert!(store.items.len() == 6 && store.items.iter().all(|f| f.source == "rerun.md"));
        store.remove(None);
        assert!(store.items.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
