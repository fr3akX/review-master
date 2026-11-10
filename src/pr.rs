//! Pull request acquisition through the GitHub CLI (`gh`): identity, metadata and
//! the unified diff. `gh` already holds the user's login, so no tokens are handled
//! here. The review itself runs over a detached worktree pinned to the PR head.

use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PrRef {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// `https://github.com/o/r/pull/N`, `o/r#N`, or `#N` / `N` against the checkout's
/// GitHub `origin`.
pub fn parse_ref(raw: &str, origin: Option<&(String, String)>) -> Result<PrRef, String> {
    let raw = raw.trim();
    let no_proto = raw.strip_prefix("https://").or_else(|| raw.strip_prefix("http://")).unwrap_or(raw);
    if let Some(rest) = no_proto.strip_prefix("github.com/").or_else(|| no_proto.strip_prefix("www.github.com/")) {
        let parts: Vec<&str> = rest.split('/').collect();
        if let [owner, repo, "pull", number, ..] = parts.as_slice() {
            let number = number.parse().map_err(|_| format!("`{number}` is not a pull request number"))?;
            return Ok(PrRef { owner: (*owner).to_owned(), repo: (*repo).to_owned(), number });
        }
        return Err(format!("`{raw}` is not a pull request URL (expected …/<owner>/<repo>/pull/<number>)"));
    }
    if let Some((slug, number)) = raw.split_once('#') {
        if let Some((owner, repo)) = slug.split_once('/') {
            let number = number.parse().map_err(|_| format!("`{number}` is not a pull request number"))?;
            return Ok(PrRef { owner: owner.to_owned(), repo: repo.to_owned(), number });
        }
    }
    let bare = raw.strip_prefix('#').unwrap_or(raw);
    if let Ok(number) = bare.parse() {
        let Some((owner, repo)) = origin else {
            return Err(format!("`{raw}` needs a repository: use <owner>/<repo>#{bare} or a URL (the checkout has no GitHub origin)"));
        };
        return Ok(PrRef { owner: owner.clone(), repo: repo.clone(), number });
    }
    Err(format!("`{raw}` is not a pull request: use a URL, <owner>/<repo>#<number>, or a number"))
}

/// GitHub `owner/repo` of the checkout's `origin` remote, any URL form.
pub fn origin_slug(root: &std::path::Path) -> Option<(String, String)> {
    let url = crate::repo::git(root, &["remote", "get-url", "origin"])?;
    let rest = url.split_once("github.com").map(|(_, r)| r)?;
    let rest = rest.strip_prefix([':', '/'])?;
    let mut parts = rest.trim_end_matches('/').splitn(2, '/');
    let owner = parts.next()?.to_owned();
    let repo = parts.next()?.trim_end_matches(".git").to_owned();
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/')).then_some((owner, repo))
}

/// Run `gh` with `args`; stderr becomes the error.
pub fn gh(args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("gh");
    cmd.args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("gh: {e} (is the GitHub CLI installed and on PATH?)"))?;
    if let Some(bytes) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        pipe.write_all(bytes).map_err(|e| format!("gh stdin: {e}"))?;
        drop(pipe);
    }
    let out = child.wait_with_output().map_err(|e| format!("gh: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = if stderr.trim().is_empty() { stdout } else { stderr };
        return Err(format!("gh {}: {}", args.join(" "), crate::engine::one_line(detail.trim(), 400)));
    }
    Ok(out.stdout)
}

pub struct PrMeta {
    pub title: String,
    pub body: String,
    pub author: String,
    pub state: String,
    pub draft: bool,
    pub merged: bool,
    pub base_ref: String,
    pub head_ref: String,
    pub head_sha: String,
    pub url: String,
}

pub fn meta(r: &PrRef) -> Result<PrMeta, String> {
    let path = format!("repos/{}/pulls/{}", r.slug(), r.number);
    let v: Value = serde_json::from_slice(&gh(&["api", &path], None)?).map_err(|e| format!("gh api {path}: {e}"))?;
    let s = |field: &Value| field.as_str().unwrap_or_default().to_owned();
    let head_sha = s(&v["head"]["sha"]);
    if head_sha.is_empty() {
        return Err(format!("gh api {path}: no head commit in the response"));
    }
    Ok(PrMeta {
        title: s(&v["title"]),
        body: s(&v["body"]),
        author: s(&v["user"]["login"]),
        state: s(&v["state"]),
        draft: v["draft"].as_bool().unwrap_or(false),
        merged: v["merged"].as_bool().unwrap_or(false),
        base_ref: s(&v["base"]["ref"]),
        head_ref: s(&v["head"]["ref"]),
        head_sha,
        url: s(&v["html_url"]),
    })
}

/// The PR's unified diff at its current head.
pub fn diff(r: &PrRef) -> Result<String, String> {
    let path = format!("repos/{}/pulls/{}", r.slug(), r.number);
    let bytes = gh(&["api", &path, "-H", "Accept: application/vnd.github.v3.diff"], None)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The checkout the review runs in: the repository itself when it already sits on
/// the PR head, else a detached temporary worktree (removed on drop).
pub struct Checkout {
    pub root: PathBuf,
    repo_root: PathBuf,
    temp: bool,
}

impl Drop for Checkout {
    fn drop(&mut self) {
        if self.temp {
            let _ = Command::new("git")
                .arg("-C")
                .arg(&self.repo_root)
                .args(["worktree", "remove", "--force"])
                .arg(&self.root)
                .output();
        }
    }
}

fn run_git(root: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), crate::engine::one_line(String::from_utf8_lossy(&out.stderr).trim(), 300)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_owned())
}

/// Make `want_sha` (the PR head) available and return the directory to review in,
/// plus the commit actually reviewed (the PR ref's tip at fetch time).
pub fn checkout(repo: &crate::repo::Repo, r: &PrRef, want_sha: &str) -> Result<(Checkout, String), String> {
    if repo.head().as_deref() == Some(want_sha) {
        return Ok((Checkout { root: repo.root.clone(), repo_root: repo.root.clone(), temp: false }, want_sha.to_owned()));
    }
    run_git(&repo.root, &["fetch", "--quiet", "origin", &format!("pull/{}/head", r.number)])?;
    let fetched = run_git(&repo.root, &["rev-parse", "FETCH_HEAD"])?;
    if fetched != want_sha {
        eprintln!("note: the pull request moved while fetching; reviewing {}", &fetched[..fetched.len().min(12)]);
    }
    let dir = std::env::temp_dir().join(format!("review-master-pr{}-{}", r.number, uuid::Uuid::new_v4()));
    run_git(&repo.root, &["worktree", "add", "--detach", dir.to_str().ok_or("temp dir is not UTF-8")?, &fetched])?;
    let root = dir.canonicalize().map_err(|e| format!("worktree {}: {e}", dir.display()))?;
    Ok((Checkout { root, repo_root: repo.root.clone(), temp: true }, fetched))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_references_parse_in_every_supported_form() {
        let origin = ("acme".to_owned(), "widgets".to_owned());
        for (raw, owner, repo, number) in [
            ("https://github.com/acme/widgets/pull/176", "acme", "widgets", 176),
            ("https://github.com/acme/widgets/pull/176/files", "acme", "widgets", 176),
            ("acme/widgets#176", "acme", "widgets", 176),
            ("#176", "acme", "widgets", 176),
            ("176", "acme", "widgets", 176),
        ] {
            let r = parse_ref(raw, Some(&origin)).unwrap();
            assert_eq!((r.owner.as_str(), r.repo.as_str(), r.number), (owner, repo, number), "{raw}");
        }
        assert!(parse_ref("176", None).unwrap_err().contains("origin"));
        assert!(parse_ref("https://github.com/acme/widgets", Some(&origin)).is_err());
        assert!(parse_ref("acme/widgets#x", Some(&origin)).is_err());
    }
}
