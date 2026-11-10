//! The reviewed checkout: identity and git state.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Project-local cache directory (sessions); ignores itself in git.
pub const CACHE_DIR_NAME: &str = ".review-master";

/// Directories that hold tool state rather than project content; never part of
/// searches or directory listings.
const TOOL_DIRS: &[&str] = &[".git", CACHE_DIR_NAME, ".metals", ".bloop", ".bsp", ".scala-build"];

pub fn is_tool_dir(name: &std::ffi::OsStr) -> bool {
    TOOL_DIRS.iter().any(|d| name == *d)
}

pub struct Repo {
    /// Canonical root directory.
    pub root: PathBuf,
    git: bool,
}

impl Repo {
    pub fn open(path: &Path) -> Result<Repo, String> {
        let root = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        let git = git(&root, &["rev-parse", "HEAD"]).is_some();
        Ok(Repo { root, git })
    }

    /// Current `HEAD`, read live.
    pub fn head(&self) -> Option<String> {
        self.git.then(|| git(&self.root, &["rev-parse", "HEAD"])).flatten()
    }
}

pub fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_owned())
}
