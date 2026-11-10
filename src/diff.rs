//! The pull request's unified diff, parsed into the positions GitHub accepts for
//! line-anchored review comments: added (`+`) and context lines by their NEW file
//! numbers on the RIGHT side, deleted (`-`) and context lines by their OLD numbers
//! on the LEFT, each tied to the hunk it appears in. Every anchor is checked here
//! before posting, so one bad finding never 422s the whole review.

use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

#[derive(Default)]
struct FileDiff {
    /// New (head) file line number → hunk index, for `+` and context lines.
    right: HashMap<u64, usize>,
    /// Old (base) file line number → hunk index, for `-` and context lines.
    left: HashMap<u64, usize>,
    hunks: usize,
}

/// Commentable positions of one unified diff.
#[derive(Default)]
pub struct Diff {
    files: HashMap<String, FileDiff>,
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

impl Diff {
    pub fn parse(text: &str) -> Diff {
        let mut diff = Diff::default();
        let mut old_path: Option<String> = None;
        let mut file: Option<String> = None;
        let (mut old_line, mut new_line) = (0u64, 0u64);
        // Lines left to consume in the current hunk, per side.
        let (mut old_left, mut new_left) = (0u64, 0u64);
        for line in text.lines() {
            if old_left == 0 && new_left == 0 {
                // Between hunks only headers matter.
                if line.starts_with("diff --git ") {
                    file = None;
                    old_path = None;
                } else if let Some(rest) = line.strip_prefix("--- ") {
                    old_path = header_path(rest);
                } else if let Some(rest) = line.strip_prefix("+++ ") {
                    // A deleted file keeps its old path: that is what GitHub comments use.
                    file = header_path(rest).or_else(|| old_path.take());
                    if let Some(path) = &file {
                        diff.files.entry(path.clone()).or_default();
                    }
                } else if let Some((old_start, old_count, new_start, new_count)) = hunk_header(line) {
                    if let Some(f) = file.as_ref().and_then(|p| diff.files.get_mut(p)) {
                        f.hunks += 1;
                        (old_line, new_line) = (old_start, new_start);
                        (old_left, new_left) = (old_count, new_count);
                    }
                }
                continue;
            }
            let Some(f) = file.as_ref().and_then(|p| diff.files.get_mut(p)) else { continue };
            let hunk = f.hunks - 1;
            match line.as_bytes().first() {
                Some(b'+') => {
                    f.right.insert(new_line, hunk);
                    new_line += 1;
                    new_left = new_left.saturating_sub(1);
                }
                Some(b'-') => {
                    f.left.insert(old_line, hunk);
                    old_line += 1;
                    old_left = old_left.saturating_sub(1);
                }
                Some(b'\\') => {} // "\ No newline at end of file"
                _ => {
                    // Context lines are commentable on both sides.
                    f.left.insert(old_line, hunk);
                    f.right.insert(new_line, hunk);
                    old_line += 1;
                    new_line += 1;
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
            }
        }
        diff
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

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

/// Per-file segments of a unified diff: (path, segment text), in order. Used to
/// drop `--exclude`d files from what the agent reviews.
pub fn split_files(text: &str) -> Vec<(String, String)> {
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut current = String::new();
    let flush = |current: &mut String, parts: &mut Vec<(String, String)>| {
        if current.is_empty() {
            return;
        }
        let mut old = None;
        let mut path = None;
        for line in current.lines() {
            if let Some(rest) = line.strip_prefix("--- ") {
                old = header_path(rest);
            } else if let Some(rest) = line.strip_prefix("+++ ") {
                path = header_path(rest).or_else(|| old.take());
                break;
            }
        }
        let path = path.unwrap_or_default();
        parts.push((path, std::mem::take(current)));
    };
    for line in text.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            flush(&mut current, &mut parts);
        }
        current.push_str(line);
    }
    flush(&mut current, &mut parts);
    parts
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
";

    #[test]
    fn added_context_and_deleted_lines_map_to_their_sides() {
        let d = Diff::parse(DIFF);
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
        let d = Diff::parse(DIFF);
        assert!(d.check("src/lib.rs", Side::Right, Some(13), 42).unwrap_err().contains("span different hunks"));
        assert!(d.check("src/lib.rs", Side::Right, Some(13), 13).unwrap_err().contains("must be smaller"));
        assert!(d.check("src/lib.rs", Side::Right, Some(41), 42).is_ok());
    }

    #[test]
    fn created_and_deleted_files_use_the_surviving_path() {
        let d = Diff::parse(DIFF);
        assert!(d.check("docs/new.md", Side::Right, Some(1), 2).is_ok());
        assert!(d.check("docs/new.md", Side::Left, None, 1).is_err());
        assert!(d.check("gone.txt", Side::Left, Some(1), 2).is_ok());
        assert!(d.check("gone.txt", Side::Right, None, 1).is_err());
    }

    #[test]
    fn split_files_keeps_every_segment_and_its_path() {
        let parts = split_files(DIFF);
        let paths: Vec<&str> = parts.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["src/lib.rs", "docs/new.md", "gone.txt"]);
        assert_eq!(parts.iter().map(|(_, s)| s.as_str()).collect::<String>(), DIFF);
        assert!(parts[1].1.starts_with("diff --git a/docs/new.md"));
    }
}
