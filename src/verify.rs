//! Path confinement. The agent's tools and citations only ever see
//! repository-relative paths resolved through here, so symlink escapes and
//! `..` traversal never leave the reviewed checkout.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// Resolve a repository-relative (or absolute, inside `root`) path. `root` must be
/// canonical. Returns the canonical path and its repository-relative display form.
/// Symlinks that leave the repository are rejected.
pub fn resolve(root: &Path, raw: &str) -> Result<(PathBuf, String), String> {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Err("empty path".into());
    }
    let given = Path::new(trimmed);
    let rel = if given.is_absolute() {
        given.strip_prefix(root).map_err(|_| "outside the repository".to_string())?
    } else {
        given
    };
    if rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err("outside the repository".into());
    }
    let full = root.join(rel).canonicalize().map_err(|_| "does not exist".to_string())?;
    let shown = full
        .strip_prefix(root)
        .map_err(|_| "outside the repository".to_string())?
        .to_string_lossy()
        .into_owned();
    Ok((full, if shown.is_empty() { ".".into() } else { shown }))
}

/// Lines in a file as diffs number them: a final newline ends the last line.
pub fn line_count(bytes: &[u8]) -> usize {
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    newlines + usize::from(bytes.last().is_some_and(|&b| b != b'\n'))
}

/// A confined, readable file's display path and line count.
pub fn file_lines(root: &Path, raw: &str) -> Result<(String, usize), String> {
    let (full, shown) = resolve(root, raw)?;
    if !full.is_file() {
        return Err("not a readable file".into());
    }
    let bytes = fs::read(&full).map_err(|_| "not a readable file".to_string())?;
    Ok((shown, line_count(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(tag: &str) -> TempRepo {
            let dir = std::env::temp_dir().join(format!("review-master-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(dir.join("src/main.rs"), "fn main() {\n    run();\n}").unwrap();
            TempRepo(dir.canonicalize().unwrap())
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn paths_are_confined_to_the_repository() {
        let repo = TempRepo::new("confine");
        let root = &repo.0;
        assert_eq!(resolve(root, "./src/main.rs").unwrap().1, "src/main.rs");
        let absolute = root.join("src/main.rs");
        assert_eq!(resolve(root, absolute.to_str().unwrap()).unwrap().1, "src/main.rs");
        assert_eq!(resolve(root, ".").unwrap().1, ".");
        assert!(resolve(root, "../etc/passwd").is_err());
        assert!(resolve(root, "src/../../etc/passwd").is_err());
        assert!(resolve(root, "/etc/passwd").is_err());
        assert!(resolve(root, "src/missing.rs").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_leaving_the_repository_are_rejected() {
        let repo = TempRepo::new("symlink");
        // The target must exist for canonicalize to succeed; the temp dir always does.
        std::os::unix::fs::symlink(std::env::temp_dir(), repo.0.join("escape")).unwrap();
        assert_eq!(resolve(&repo.0, "escape").unwrap_err(), "outside the repository");
    }

    #[test]
    fn lines_are_counted_like_diff_line_numbers() {
        for (text, want) in [("", 0), ("a", 1), ("a\n", 1), ("a\nb", 2), ("a\nb\n", 2), ("\n", 1), ("a\n\n", 2)] {
            assert_eq!(line_count(text.as_bytes()), want, "{text:?}");
        }
    }
}
