//! Reviewing a worktree without a pull/merge request: everything the checkout has that
//! its base does not — commits since the merge base, staged and unstaged edits, and
//! untracked files (`.gitignore` respected). The base is the default branch, or any
//! ref (`HEAD`: only what is not committed yet). The change is re-read on demand while
//! the reviewer keeps editing, and the review is written out as Markdown.

use crate::diff::{self, Model};
use std::path::Path;
use std::process::{Command, Stdio};

/// Untracked files diffed at most; a forgotten build directory should not drown the review.
const MAX_UNTRACKED: usize = 200;

/// What a local review compares: `base` (`None`: the default branch) and the
/// `--exclude` globs, kept to re-read the change.
#[derive(Clone, Debug)]
pub struct Source {
    pub base: Option<String>,
    pub exclude: Vec<String>,
}

pub struct LocalChange {
    pub model: Model,
    /// Files `--exclude` dropped.
    pub skipped: Vec<String>,
    pub head: String,
    pub branch: String,
    /// Uncommitted edits or untracked files are part of the change.
    pub dirty: bool,
    /// The base as the reviewer knows it: `origin/main`, `HEAD`, a ref they gave.
    pub base: String,
    /// Untracked files beyond [`MAX_UNTRACKED`], left out.
    pub untracked_left_out: usize,
    /// Identifies the change's content (head and diff), for cached summaries and flows.
    pub key: String,
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).stdin(Stdio::null()).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), crate::engine::one_line(String::from_utf8_lossy(&out.stderr).trim(), 300)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_owned())
}

/// The default branch: `origin/HEAD`'s target, else `origin/main`, `origin/master`, `main`, `master`.
fn default_branch(root: &Path) -> Result<String, String> {
    if let Ok(name) = git(root, &["rev-parse", "--abbrev-ref", "refs/remotes/origin/HEAD"]) {
        if !name.is_empty() && name != "refs/remotes/origin/HEAD" {
            return Ok(name);
        }
    }
    ["origin/main", "origin/master", "main", "master"]
        .iter()
        .find(|r| git(root, &["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")]).is_ok())
        .map(|r| (*r).to_owned())
        .ok_or_else(|| "cannot find the default branch (origin/HEAD, main or master); name one with --base".to_owned())
}

/// A new-file diff for each untracked file (at most [`MAX_UNTRACKED`]); how many were left out.
fn untracked_diffs(root: &Path) -> Result<(String, usize), String> {
    let listed = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut paths: Vec<&str> = listed.split('\0').filter(|p| !p.is_empty()).collect();
    paths.sort_unstable();
    let left_out = paths.len().saturating_sub(MAX_UNTRACKED);
    let mut text = String::new();
    for path in paths.into_iter().take(MAX_UNTRACKED) {
        // Exit status 1 means "differs", which is the point.
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["diff", "--no-index", "--no-color", "--", "/dev/null", path])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("git: {e}"))?;
        if out.status.code() == Some(1) {
            text.push_str(&String::from_utf8_lossy(&out.stdout));
            if !text.ends_with('\n') {
                text.push('\n');
            }
        }
    }
    Ok((text, left_out))
}

/// FNV-1a: stable across runs and Rust versions, unlike `DefaultHasher`.
fn fingerprint(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

impl Source {
    /// The worktree's change against the base, untracked files included.
    pub fn read(&self, root: &Path) -> Result<LocalChange, String> {
        let head = git(root, &["rev-parse", "HEAD"]).map_err(|_| format!("{} is not a git checkout with a commit", root.display()))?;
        let branch = git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "detached".to_owned());
        let base = match &self.base {
            Some(b) => {
                git(root, &["rev-parse", "--verify", "--quiet", &format!("{b}^{{commit}}")]).map_err(|_| format!("--base {b}: not a commit in this repository"))?;
                b.clone()
            }
            None => default_branch(root)?,
        };
        let merge_base = git(root, &["merge-base", &base, "HEAD"])?;
        let tracked = git(root, &["diff", "-M", "--no-color", &merge_base])?;
        let (untracked, untracked_left_out) = untracked_diffs(root)?;
        let mut text = tracked;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&untracked);
        if text.trim().is_empty() {
            let what = if base == "HEAD" { "no uncommitted edits or untracked files".to_owned() } else { format!("no change against {base} (commits, edits or untracked files)") };
            return Err(format!("nothing to review: {what}"));
        }
        let dirty = !git(root, &["status", "--porcelain"]).unwrap_or_default().is_empty();
        let key = format!("{head}:{:016x}", fingerprint(&text));
        let mut model = Model::parse(&text);
        let skipped = diff::apply_excludes(&mut model, &self.exclude)?;
        Ok(LocalChange { model, skipped, head, branch, dirty, base, untracked_left_out, key })
    }
}

/// A saved draft, as the Markdown export needs it.
pub struct Comment<'a> {
    pub path: &'a str,
    pub start: Option<u64>,
    pub line: u64,
    /// `LEFT`: lines the change removes (numbered in the base).
    pub left: bool,
    pub severity: &'a str,
    pub body: &'a str,
}

/// A heading title from a comment's first line, without Markdown emphasis.
fn title(body: &str) -> String {
    let first = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Comment");
    let plain: String = first.trim_start_matches(['#', '>', '-', '*', ' ']).chars().filter(|c| !matches!(c, '*' | '_' | '`')).collect();
    let plain = plain.trim();
    if plain.chars().count() > 80 {
        format!("{}…", plain.chars().take(79).collect::<String>())
    } else if plain.is_empty() {
        "Comment".to_owned()
    } else {
        plain.to_owned()
    }
}

/// The review as Markdown: a heading per comment in the shape `--findings` imports
/// (`### [severity] title (path:line)`), so an agent or a later review can take it up.
pub fn markdown(branch: &str, base: &str, head: &str, summary: &str, comments: &[Comment]) -> String {
    let against = if base == "HEAD" { "uncommitted changes".to_owned() } else { format!("against `{base}`") };
    let mut out = format!("# Review of `{branch}` ({against}, at {})\n\n", &head[..head.len().min(12)]);
    if !summary.trim().is_empty() {
        out.push_str(summary.trim());
        out.push_str("\n\n");
    }
    for c in comments {
        let lines = match c.start {
            Some(s) if s != c.line => format!("{s}-{}", c.line),
            _ => c.line.to_string(),
        };
        let place = if c.left { format!("{}, removed line {lines}", c.path) } else { format!("{}:{lines}", c.path) };
        out.push_str(&format!("### [{}] {} ({place})\n\n{}\n\n", c.severity, title(c.body), c.body.trim()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(root: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(root).args(args).output().unwrap().status.success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn local_change_covers_commits_edits_and_untracked_files_against_its_base() {
        let dir = std::env::temp_dir().join(format!("rm-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "-q", "-b", "main"]);
        run(&dir, &["config", "user.email", "t@example.com"]);
        run(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-qm", "base"]);
        run(&dir, &["checkout", "-qb", "work"]);
        std::fs::write(dir.join("b.txt"), "committed\n").unwrap();
        run(&dir, &["add", "b.txt"]);
        run(&dir, &["commit", "-qm", "work"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::create_dir_all(dir.join("new dir")).unwrap();
        std::fs::write(dir.join("new dir/c.txt"), "untracked\n").unwrap();
        std::fs::write(dir.join("noise.log"), "ignored\n").unwrap();

        let all = Source { base: None, exclude: vec![] }.read(&dir).unwrap();
        let paths: Vec<&str> = all.model.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b.txt", "new dir/c.txt"]);
        assert_eq!((all.base.as_str(), all.branch.as_str(), all.dirty), ("main", "work", true));

        let uncommitted = Source { base: Some("HEAD".into()), exclude: vec!["*.txt".into()] }.read(&dir).unwrap();
        assert!(uncommitted.model.files.is_empty() && uncommitted.skipped == ["a.txt", "new dir/c.txt"]);

        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let again = Source { base: None, exclude: vec![] }.read(&dir).unwrap();
        assert_ne!(again.key, all.key, "an edit changes the key");
        assert_eq!(Source { base: None, exclude: vec![] }.read(&dir).unwrap().key, again.key);

        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-qm", "all"]);
        let err = Source { base: Some("HEAD".into()), exclude: vec![] }.read(&dir).err().unwrap();
        assert!(err.contains("no uncommitted edits"), "{err}");
        assert!(Source { base: Some("nope".into()), exclude: vec![] }.read(&dir).err().unwrap().contains("--base nope"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn markdown_review_uses_the_findings_shape() {
        let comments = [
            Comment { path: "src/a.rs", start: Some(3), line: 5, left: false, severity: "major", body: "**Leaks** the handle.\n\nClose it." },
            Comment { path: "src/b.rs", start: None, line: 9, left: true, severity: "nit", body: "Why remove this?" },
        ];
        let md = markdown("work", "HEAD", "0123456789abcdef", "Mostly fine.", &comments);
        assert!(md.starts_with("# Review of `work` (uncommitted changes, at 0123456789ab)\n\nMostly fine.\n\n"), "{md}");
        assert!(md.contains("### [major] Leaks the handle. (src/a.rs:3-5)\n\n**Leaks** the handle.\n\nClose it.\n"), "{md}");
        assert!(md.contains("### [nit] Why remove this? (src/b.rs, removed line 9)"), "{md}");
        let mut store = crate::findings::Store::default();
        let imported = store.import("review.md", &md, Path::new("."), &Model::parse(""));
        // A removed line has no head line to sit on: readable in the file, not imported.
        assert_eq!((imported.added, imported.skipped.len()), (1, 1), "{md}");
    }
}
