//! The reviewed change as a structured model: files → hunks → lines, parsed from
//! a unified diff (GitHub's or git's). The anchor index derived from it holds the
//! positions GitHub accepts for line-anchored review comments: added (`+`) and
//! context lines by their NEW file numbers on the RIGHT side, deleted (`-`) and
//! context lines by their OLD numbers on the LEFT, each tied to its hunk. Every
//! anchor is checked before posting, so one bad finding never 422s the review.

use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn api(self) -> &'static str {
        match self {
            Side::Left => "LEFT",
            Side::Right => "RIGHT",
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.api())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl Status {
    pub fn api(self) -> &'static str {
        match self {
            Status::Added => "added",
            Status::Modified => "modified",
            Status::Deleted => "deleted",
            Status::Renamed => "renamed",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Ctx,
    Add,
    Del,
}

#[derive(Clone)]
pub struct Line {
    pub kind: Kind,
    /// Old-file line number (`None` for added lines).
    pub old: Option<u64>,
    /// New-file line number (`None` for deleted lines).
    pub new: Option<u64>,
    /// Without the `+`/`-`/space prefix.
    pub text: String,
}

#[derive(Clone)]
pub struct Hunk {
    /// The original `@@ … @@ …` header line, verbatim.
    pub header: String,
    pub lines: Vec<Line>,
}

#[derive(Clone)]
pub struct FileEntry {
    /// The surviving path (new side; old side for deletions) — what GitHub comments use.
    pub path: String,
    /// The old path when renamed.
    pub old_path: Option<String>,
    pub status: Status,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
}

/// The whole change, in diff order.
#[derive(Clone, Default)]
pub struct Model {
    pub files: Vec<FileEntry>,
}

/// `+++ b/path` / `--- a/path` → repository-relative path; `/dev/null` → `None`.
fn header_path(rest: &str) -> Option<String> {
    let path = rest.split('\t').next().unwrap_or(rest).trim_end();
    if path == "/dev/null" {
        return None;
    }
    let path = path.strip_prefix("a/").or_else(|| path.strip_prefix("b/")).unwrap_or(path);
    Some(path.to_owned())
}

/// `@@ -old[,n] +new[,m] @@ …` → (old start, old count, new start, new count).
fn hunk_header(line: &str) -> Option<(u64, u64, u64, u64)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let parse = |spec: &str| -> Option<(u64, u64)> {
        match spec.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((spec.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = parse(old)?;
    let (new_start, new_count) = parse(new)?;
    Some((old_start, old_count, new_start, new_count))
}

/// One file being assembled, before its identity is settled.
#[derive(Default)]
struct Pending {
    old: Option<String>,
    new: Option<String>,
    rename_from: Option<String>,
    rename_to: Option<String>,
    created: bool,
    deleted: bool,
    hunks: Vec<Hunk>,
}

impl Pending {
    fn settle(self) -> Option<FileEntry> {
        let (old, new) = (
            self.old.or(self.rename_from.clone()),
            self.new.or(self.rename_to.clone()),
        );
        let (path, status, old_path) = match (&old, &new) {
            (None, Some(n)) => (n.clone(), Status::Added, None),
            (Some(o), None) => (o.clone(), Status::Deleted, None),
            (Some(o), Some(n)) if o != n => (n.clone(), Status::Renamed, Some(o.clone())),
            (Some(_), Some(n)) => {
                let status = if self.created {
                    Status::Added
                } else if self.deleted {
                    Status::Deleted
                } else {
                    Status::Modified
                };
                (n.clone(), status, None)
            }
            (None, None) => return None,
        };
        let additions = self.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == Kind::Add).count() as u32;
        let deletions = self.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == Kind::Del).count() as u32;
        Some(FileEntry { path, old_path, status, hunks: self.hunks, additions, deletions })
    }
}

impl Model {
    pub fn parse(text: &str) -> Model {
        let mut model = Model::default();
        let mut pending: Option<Pending> = None;
        let (mut old_line, mut new_line) = (0u64, 0u64);
        // Lines left to consume in the current hunk, per side.
        let (mut old_left, mut new_left) = (0u64, 0u64);
        let flush = |p: &mut Option<Pending>, model: &mut Model| {
            if let Some(file) = p.take().and_then(Pending::settle) {
                model.files.push(file);
            }
        };
        for line in text.lines() {
            if old_left == 0 && new_left == 0 {
                // Between hunks only headers matter.
                if line.starts_with("diff --git ") {
                    flush(&mut pending, &mut model);
                    pending = Some(Pending::default());
                } else if let Some(rest) = line.strip_prefix("--- ") {
                    // Headerless diffs (bare `---`/`+++` pairs) start a file here.
                    pending.get_or_insert_with(Pending::default).old = header_path(rest);
                } else if let Some(p) = pending.as_mut() {
                    if let Some(rest) = line.strip_prefix("+++ ") {
                        p.new = header_path(rest);
                    } else if let Some(rest) = line.strip_prefix("rename from ") {
                        p.rename_from = Some(rest.to_owned());
                    } else if let Some(rest) = line.strip_prefix("rename to ") {
                        p.rename_to = Some(rest.to_owned());
                    } else if line.starts_with("new file mode ") {
                        p.created = true;
                    } else if line.starts_with("deleted file mode ") {
                        p.deleted = true;
                    } else if let Some((old_start, old_count, new_start, new_count)) = hunk_header(line) {
                        p.hunks.push(Hunk { header: line.to_owned(), lines: Vec::new() });
                        (old_line, new_line) = (old_start, new_start);
                        (old_left, new_left) = (old_count, new_count);
                    }
                }
                continue;
            }
            let Some(hunk) = pending.as_mut().and_then(|p| p.hunks.last_mut()) else { continue };
            let (kind, old, new, text) = match line.as_bytes().first() {
                Some(b'+') => {
                    let l = (Kind::Add, None, Some(new_line), line[1..].to_owned());
                    new_line += 1;
                    new_left = new_left.saturating_sub(1);
                    l
                }
                Some(b'-') => {
                    let l = (Kind::Del, Some(old_line), None, line[1..].to_owned());
                    old_line += 1;
                    old_left = old_left.saturating_sub(1);
                    l
                }
                Some(b'\\') => continue, // "\ No newline at end of file"
                _ => {
                    let l = (Kind::Ctx, Some(old_line), Some(new_line), line.get(1..).unwrap_or("").to_owned());
                    old_line += 1;
                    new_line += 1;
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    l
                }
            };
            hunk.lines.push(Line { kind, old, new, text });
        }
        flush(&mut pending, &mut model);
        model
    }

    pub fn file(&self, path: &str) -> Option<&FileEntry> {
        self.files.iter().find(|f| f.path == path)
    }

    /// Drop files `keep` rejects, in place; returns the dropped paths.
    pub fn retain(&mut self, mut keep: impl FnMut(&FileEntry) -> bool) -> Vec<String> {
        let mut dropped = Vec::new();
        self.files.retain(|f| {
            let kept = keep(f);
            if !kept {
                dropped.push(f.path.clone());
            }
            kept
        });
        dropped
    }

    /// Faithful unified diff text of one file (for prompts).
    pub fn render_file(file: &FileEntry, out: &mut String) {
        let old = file.old_path.as_deref().unwrap_or(&file.path);
        let (a, b) = match file.status {
            Status::Added => ("/dev/null".to_owned(), format!("b/{}", file.path)),
            Status::Deleted => (format!("a/{}", file.path), "/dev/null".to_owned()),
            _ => (format!("a/{old}"), format!("b/{}", file.path)),
        };
        out.push_str(&format!("diff --git a/{old} b/{}\n--- {a}\n+++ {b}\n", file.path));
        for hunk in &file.hunks {
            out.push_str(&hunk.header);
            out.push('\n');
            for line in &hunk.lines {
                out.push(match line.kind {
                    Kind::Ctx => ' ',
                    Kind::Add => '+',
                    Kind::Del => '-',
                });
                out.push_str(&line.text);
                out.push('\n');
            }
        }
    }

    /// The anchor index for comment validation.
    pub fn anchors(&self) -> Diff {
        let mut files = HashMap::new();
        for file in &self.files {
            let entry: &mut FileAnchors = files.entry(file.path.clone()).or_default();
            for (h, hunk) in file.hunks.iter().enumerate() {
                for line in &hunk.lines {
                    if let Some(old) = line.old {
                        entry.left.insert(old, h);
                    }
                    if let Some(new) = line.new {
                        entry.right.insert(new, h);
                    }
                }
            }
        }
        Diff { files }
    }
}

#[derive(Default)]
struct FileAnchors {
    /// New (head) file line number → hunk index, for `+` and context lines.
    right: HashMap<u64, usize>,
    /// Old (base) file line number → hunk index, for `-` and context lines.
    left: HashMap<u64, usize>,
}

/// Commentable positions of the reviewed diff.
#[derive(Default)]
pub struct Diff {
    files: HashMap<String, FileAnchors>,
}

impl Diff {
    /// `Ok` means GitHub will accept `start..=line` on `side` as an inline position.
    pub fn check(&self, path: &str, side: Side, start: Option<u64>, line: u64) -> Result<(), String> {
        let Some(f) = self.files.get(path) else {
            return Err(format!("`{path}` is not part of the reviewed diff"));
        };
        let lines = match side {
            Side::Left => &f.left,
            Side::Right => &f.right,
        };
        let Some(&hunk) = lines.get(&line) else {
            return Err(format!("{side} line {line} of `{path}` is not shown in the diff"));
        };
        if let Some(start) = start {
            if start >= line {
                return Err(format!("start_line {start} must be smaller than line {line}"));
            }
            match lines.get(&start) {
                None => return Err(format!("{side} line {start} of `{path}` is not shown in the diff")),
                Some(&h) if h != hunk => return Err(format!("lines {start}-{line} of `{path}` span different hunks; comment on one hunk")),
                Some(_) => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,3 +10,4 @@ fn ten() {}
 context10
-removed11
+added11
+added12
 context13
@@ -40,2 +41,2 @@
 context41
-removed41
+added42
diff --git a/docs/new.md b/docs/new.md
new file mode 100644
--- /dev/null
+++ b/docs/new.md
@@ -0,0 +1,2 @@
+first
+second
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-one
-two
diff --git a/old/name.rs b/new/name.rs
similarity index 90%
rename from old/name.rs
rename to new/name.rs
--- a/old/name.rs
+++ b/new/name.rs
@@ -3,1 +3,1 @@
-fn old() {}
+fn renamed() {}
";

    #[test]
    fn the_model_captures_files_statuses_and_numbered_lines() {
        let m = Model::parse(DIFF);
        let statuses: Vec<(&str, Status)> = m.files.iter().map(|f| (f.path.as_str(), f.status)).collect();
        assert_eq!(
            statuses,
            [
                ("src/lib.rs", Status::Modified),
                ("docs/new.md", Status::Added),
                ("gone.txt", Status::Deleted),
                ("new/name.rs", Status::Renamed),
            ]
        );
        let lib = m.file("src/lib.rs").unwrap();
        assert_eq!((lib.additions, lib.deletions), (3, 2));
        assert_eq!(lib.hunks[0].header, "@@ -10,3 +10,4 @@ fn ten() {}");
        let l = &lib.hunks[0].lines;
        assert_eq!((l[0].kind, l[0].old, l[0].new, l[0].text.as_str()), (Kind::Ctx, Some(10), Some(10), "context10"));
        assert_eq!((l[1].kind, l[1].old, l[1].new), (Kind::Del, Some(11), None));
        assert_eq!((l[2].kind, l[2].old, l[2].new), (Kind::Add, None, Some(11)));
        assert_eq!((l[4].kind, l[4].old, l[4].new), (Kind::Ctx, Some(12), Some(13)));
        assert_eq!(m.file("new/name.rs").unwrap().old_path.as_deref(), Some("old/name.rs"));
    }

    #[test]
    fn added_context_and_deleted_lines_map_to_their_sides() {
        let d = Model::parse(DIFF).anchors();
        // Context and added lines anchor on the RIGHT with new numbering.
        assert!(d.check("src/lib.rs", Side::Right, None, 10).is_ok());
        assert!(d.check("src/lib.rs", Side::Right, None, 11).is_ok());
        assert!(d.check("src/lib.rs", Side::Right, None, 12).is_ok());
        assert!(d.check("src/lib.rs", Side::Right, None, 13).is_ok());
        // The deleted line only exists on the LEFT with old numbering.
        assert!(d.check("src/lib.rs", Side::Left, None, 11).is_ok());
        assert!(d.check("src/lib.rs", Side::Right, Some(10), 13).is_ok());
        // Lines the diff does not show are rejected.
        assert!(d.check("src/lib.rs", Side::Right, None, 14).is_err());
        assert!(d.check("src/lib.rs", Side::Right, None, 9).is_err());
        assert!(d.check("other.rs", Side::Right, None, 1).is_err());
    }

    #[test]
    fn ranges_must_stay_inside_one_hunk_and_run_forward() {
        let d = Model::parse(DIFF).anchors();
        assert!(d.check("src/lib.rs", Side::Right, Some(13), 42).unwrap_err().contains("span different hunks"));
        assert!(d.check("src/lib.rs", Side::Right, Some(13), 13).unwrap_err().contains("must be smaller"));
        assert!(d.check("src/lib.rs", Side::Right, Some(41), 42).is_ok());
    }

    #[test]
    fn created_and_deleted_files_use_the_surviving_path() {
        let d = Model::parse(DIFF).anchors();
        assert!(d.check("docs/new.md", Side::Right, Some(1), 2).is_ok());
        assert!(d.check("docs/new.md", Side::Left, None, 1).is_err());
        assert!(d.check("gone.txt", Side::Left, Some(1), 2).is_ok());
        assert!(d.check("gone.txt", Side::Right, None, 1).is_err());
    }
}
