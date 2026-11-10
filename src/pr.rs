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

/// Review comments already on the PR, oldest first. `line` is null for comments
/// whose code has since changed (outdated); `original_line` still says where.
pub fn review_comments(r: &PrRef) -> Result<Vec<Value>, String> {
    let path = format!("repos/{}/pulls/{}/comments?per_page=100", r.slug(), r.number);
    let out = gh(&["api", &path, "--paginate", "--jq", ".[]"], None)?;
    parse_review_comments(&out).map_err(|e| format!("gh api {path}: {e}"))
}

/// Post `body` as a reply in the review thread started by comment `comment`.
pub fn reply(r: &PrRef, comment: u64, body: &str) -> Result<(), String> {
    let path = format!("repos/{}/pulls/{}/comments/{comment}/replies", r.slug(), r.number);
    let payload = serde_json::to_vec(&serde_json::json!({ "body": body })).map_err(|e| e.to_string())?;
    gh(&["api", &path, "-X", "POST", "--input", "-"], Some(&payload)).map(|_| ())
}

/// One JSON object per comment, concatenated (what `--paginate --jq '.[]'` prints).
fn parse_review_comments(out: &[u8]) -> Result<Vec<Value>, String> {
    let mut list = Vec::new();
    for v in serde_json::Deserializer::from_slice(out).into_iter::<Value>() {
        let v = v.map_err(|e| e.to_string())?;
        list.push(serde_json::json!({
            "id": v["id"],
            "reply_to": v["in_reply_to_id"],
            "path": v["path"],
            "line": v["line"],
            "original_line": v["original_line"],
            "start": v["start_line"],
            "side": v["side"].as_str().unwrap_or("RIGHT"),
            "author": v["user"]["login"],
            "body": v["body"],
            "created": v["created_at"],
            "url": v["html_url"],
        }));
    }
    Ok(list)
}

/// The logged-in `gh` user, for own-PR hints in the UI.
pub fn viewer() -> Option<String> {
    let v: Value = serde_json::from_slice(&gh(&["api", "user"], None).ok()?).ok()?;
    v["login"].as_str().map(str::to_owned)
}

/// The open PR whose head is `branch`, if any.
pub fn pr_for_branch(slug: &str, branch: &str) -> Option<u64> {
    let out = gh(&["pr", "list", "--repo", slug, "--head", branch, "--state", "open", "--json", "number", "--jq", ".[0].number"], None).ok()?;
    String::from_utf8_lossy(&out).trim().parse().ok()
}

/// The open PR this checkout corresponds to: by branch name, then by the
/// branch's upstream (scratch worktrees track the real branch under another
/// local name), then by exact head commit.
pub fn resolve_pr(root: &std::path::Path, slug: &str) -> Option<u64> {
    let branch = crate::repo::git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD");
    if let Some(b) = &branch {
        if let Some(n) = pr_for_branch(slug, b) {
            println!("review-master: resolved open PR #{n} from branch `{b}`");
            return Some(n);
        }
    }
    if let Some(upstream) = crate::repo::git(root, &["rev-parse", "--abbrev-ref", "@{upstream}"]) {
        if let Some(b) = upstream.split_once('/').map(|(_, b)| b).filter(|b| Some(*b) != branch.as_deref()) {
            if let Some(n) = pr_for_branch(slug, b) {
                println!("review-master: resolved open PR #{n} from upstream `{upstream}`");
                return Some(n);
            }
        }
    }
    let sha = crate::repo::git(root, &["rev-parse", "HEAD"])?;
    let jq = format!(".[] | select(.headRefOid==\"{sha}\") | .number");
    let out = gh(&["pr", "list", "--repo", slug, "--state", "open", "--limit", "100", "--json", "number,headRefOid", "--jq", &jq], None).ok()?;
    let n = String::from_utf8_lossy(&out).split_whitespace().next()?.parse().ok()?;
    println!("review-master: resolved open PR #{n} from head commit {}", &sha[..sha.len().min(12)]);
    Some(n)
}

/// A worktree reviewed without a PR: its diff against the merge base with the
/// default branch, including uncommitted edits.
pub struct LocalChange {
    pub diff_text: String,
    pub head: String,
    pub branch: String,
    pub dirty: bool,
}

pub fn local_change(root: &std::path::Path) -> Result<LocalChange, String> {
    let head = run_git(root, &["rev-parse", "HEAD"]).map_err(|_| format!("{} is not a git checkout", root.display()))?;
    let branch = run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "detached".to_owned());
    let default = ["refs/remotes/origin/HEAD", "origin/main", "origin/master", "main", "master"]
        .iter()
        .find_map(|r| run_git(root, &["rev-parse", "--verify", "--quiet", r]).ok())
        .ok_or("cannot find the default branch (origin/HEAD, main or master); review a PR instead")?;
    let base = run_git(root, &["merge-base", &default, "HEAD"])?;
    if base == head {
        let dirty = !run_git(root, &["status", "--porcelain"]).unwrap_or_default().is_empty();
        if !dirty {
            return Err("the worktree has no change against the default branch".into());
        }
    }
    let diff_text = run_git(root, &["diff", "-M", &base])?;
    if diff_text.trim().is_empty() {
        return Err("the worktree has no change against the default branch".into());
    }
    let dirty = !run_git(root, &["status", "--porcelain"]).unwrap_or_default().is_empty();
    Ok(LocalChange { diff_text, head, branch, dirty })
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

    #[test]
    fn review_comments_parse_from_concatenated_objects() {
        let out = br#"{"id":1,"in_reply_to_id":null,"path":"a.rs","line":7,"original_line":7,"side":"LEFT","start_line":null,"user":{"login":"alice"},"body":"hm","created_at":"2026-01-01T00:00:00Z","html_url":"u1"}
{"id":2,"in_reply_to_id":1,"path":"a.rs","line":null,"original_line":7,"user":{"login":"bob"},"body":"ok"}"#;
        let list = parse_review_comments(out).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!((list[0]["author"].as_str(), list[0]["side"].as_str(), list[0]["line"].as_u64()), (Some("alice"), Some("LEFT"), Some(7)));
        // An outdated reply: no current line, side defaults to RIGHT.
        assert_eq!((list[1]["reply_to"].as_u64(), list[1]["side"].as_str()), (Some(1), Some("RIGHT")));
        assert!(list[1]["line"].is_null());
        assert!(parse_review_comments(b"").unwrap().is_empty());
        assert!(parse_review_comments(b"{oops").is_err());
    }
}
