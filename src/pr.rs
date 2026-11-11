//! Pull / merge request acquisition through the forge's CLI: GitHub via `gh`, GitLab
//! via `glab` (gitlab.com or a self-hosted instance). The CLIs already hold the
//! user's login, so no tokens are handled here. Identity, metadata, the unified diff
//! and existing review comments come from the forge; the review itself runs over a
//! detached worktree pinned to the request's head.

use crate::gitlab;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where the change is hosted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Forge {
    Github,
    Gitlab,
}

impl Forge {
    pub fn id(self) -> &'static str {
        match self {
            Forge::Github => "github",
            Forge::Gitlab => "gitlab",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Forge::Github => "GitHub",
            Forge::Gitlab => "GitLab",
        }
    }

    /// "pull request" or "merge request".
    pub fn noun(self) -> &'static str {
        match self {
            Forge::Github => "pull request",
            Forge::Gitlab => "merge request",
        }
    }
}

/// A pull request (GitHub) or merge request (GitLab). On GitLab `owner` is the
/// namespace path, which may hold subgroups (`group/sub`).
#[derive(Clone, Debug)]
pub struct PrRef {
    pub forge: Forge,
    /// `github.com`, `gitlab.com` or a self-hosted GitLab.
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PrRef {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// `#176` or `!12`.
    pub fn number_label(&self) -> String {
        match self.forge {
            Forge::Github => format!("#{}", self.number),
            Forge::Gitlab => format!("!{}", self.number),
        }
    }

    /// `acme/widgets#176`, `group/project!12`.
    pub fn label(&self) -> String {
        format!("{}{}", self.slug(), self.number_label())
    }

    /// The ref the forge keeps the request's head under, for `git fetch`.
    fn head_ref(&self) -> String {
        match self.forge {
            Forge::Github => format!("pull/{}/head", self.number),
            Forge::Gitlab => format!("merge-requests/{}/head", self.number),
        }
    }
}

/// A git remote's host and project path (`owner/repo`, or `group/sub/project`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub host: String,
    pub path: String,
    pub forge: Option<Forge>,
}

impl Remote {
    fn split(&self) -> Option<(String, String)> {
        let (owner, repo) = self.path.rsplit_once('/')?;
        (!owner.is_empty() && !repo.is_empty()).then(|| (owner.to_owned(), repo.to_owned()))
    }
}

/// Host and path of a remote URL in any form git accepts: `https://host/p.git`,
/// `ssh://git@host:2222/p.git`, `git@host:p.git`.
fn parse_remote_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?.split(':').next()?;
        (host, path)
    } else {
        // scp-like: [user@]host:path
        let (left, path) = url.split_once(':')?;
        (left.rsplit('@').next()?, path)
    };
    let path = path.trim_matches('/').trim_end_matches(".git").trim_matches('/');
    (!host.is_empty() && path.contains('/')).then(|| (host.to_ascii_lowercase(), path.to_owned()))
}

/// The forge a host runs: `--forge` when given, github.com, a host named like GitLab,
/// or a host the GitLab CLI is logged in to (self-hosted instances).
pub fn forge_of(host: &str, forced: Option<Forge>) -> Option<Forge> {
    if forced.is_some() {
        return forced;
    }
    if host == "github.com" || host == "www.github.com" {
        return Some(Forge::Github);
    }
    if host.split('.').any(|label| label.contains("gitlab")) || gitlab::logged_in(host) {
        return Some(Forge::Gitlab);
    }
    None
}

/// The checkout's `origin` remote, with the forge it is on.
pub fn origin(root: &Path, forced: Option<Forge>) -> Option<Remote> {
    let url = crate::repo::git(root, &["remote", "get-url", "origin"])?;
    let (host, path) = parse_remote_url(&url)?;
    let forge = forge_of(&host, forced);
    Some(Remote { host, path, forge })
}

/// `https://github.com/o/r/pull/N`, `https://<gitlab>/g/p/-/merge_requests/N`,
/// `o/r#N`, `g/p!N`, or `#N` / `!N` / `N` against the checkout's `origin`.
pub fn parse_ref(raw: &str, origin: Option<&Remote>, forced: Option<Forge>) -> Result<PrRef, String> {
    let raw = raw.trim();
    let number = |n: &str| n.parse::<u64>().map_err(|_| format!("`{n}` is not a pull/merge request number"));
    if let Some((_, rest)) = raw.split_once("://") {
        let (host, path) = rest.split_once('/').ok_or_else(|| format!("`{raw}` has no path"))?;
        let host = host.rsplit('@').next().unwrap_or(host).to_ascii_lowercase();
        if let Some((project, tail)) = path.split_once("/-/merge_requests/") {
            let n = number(tail.split(['/', '#', '?']).next().unwrap_or(""))?;
            let (owner, repo) = project.rsplit_once('/').ok_or_else(|| format!("`{raw}` names no project"))?;
            return Ok(PrRef { forge: Forge::Gitlab, host, owner: owner.to_owned(), repo: repo.to_owned(), number: n });
        }
        let parts: Vec<&str> = path.split('/').collect();
        if let [owner, repo, "pull", n, ..] = parts.as_slice() {
            if forge_of(&host, forced) == Some(Forge::Github) {
                return Ok(PrRef { forge: Forge::Github, host, owner: (*owner).to_owned(), repo: (*repo).to_owned(), number: number(n)? });
            }
        }
        return Err(format!("`{raw}` is not a pull request URL (…/<owner>/<repo>/pull/<number>) or a merge request URL (…/<project>/-/merge_requests/<number>)"));
    }
    // `group/project!N` (GitLab) or `owner/repo#N` (the origin's forge, else GitHub).
    for (sep, default) in [('!', Forge::Gitlab), ('#', Forge::Github)] {
        if let Some((slug, n)) = raw.split_once(sep) {
            if let Some((owner, repo)) = slug.rsplit_once('/') {
                let on_origin = origin.filter(|o| o.forge.is_some() && (sep == '#' || o.forge == Some(Forge::Gitlab)));
                let forge = forced.or(on_origin.and_then(|o| o.forge)).unwrap_or(default);
                let host = match on_origin {
                    Some(o) if o.forge == Some(forge) => o.host.clone(),
                    _ if forge == Forge::Github => "github.com".to_owned(),
                    _ => "gitlab.com".to_owned(),
                };
                return Ok(PrRef { forge, host, owner: owner.to_owned(), repo: repo.to_owned(), number: number(n)? });
            }
        }
    }
    let bare = raw.trim_start_matches(['#', '!']);
    if let Ok(n) = bare.parse() {
        let Some(o) = origin else {
            return Err(format!("`{raw}` needs a repository: use <owner>/<repo>#{bare}, <project>!{bare} or a URL (the checkout has no origin)"));
        };
        let forge = o.forge.ok_or_else(|| format!("`{raw}`: cannot tell whether {} is GitHub or GitLab; pass --forge", o.host))?;
        let (owner, repo) = o.split().ok_or_else(|| format!("`{raw}`: the origin {}/{} names no project", o.host, o.path))?;
        return Ok(PrRef { forge, host: o.host.clone(), owner, repo, number: n });
    }
    Err(format!("`{raw}` is not a pull/merge request: use a URL, <owner>/<repo>#<number>, <project>!<number>, or a number"))
}

/// Run `gh` with `args`; stderr becomes the error.
pub fn gh(args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, String> {
    run_cli("gh", "the GitHub CLI", args, stdin)
}

/// Run a forge CLI; stderr (or stdout when stderr is empty) becomes the error.
pub fn run_cli(bin: &str, what: &str, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("{bin}: {e} (is {what} installed and on PATH?)"))?;
    if let Some(bytes) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        pipe.write_all(bytes).map_err(|e| format!("{bin} stdin: {e}"))?;
        drop(pipe);
    }
    let out = child.wait_with_output().map_err(|e| format!("{bin}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = if stderr.trim().is_empty() { stdout } else { stderr };
        return Err(format!("{bin} {}: {}", args.join(" "), crate::engine::one_line(detail.trim(), 400)));
    }
    Ok(out.stdout)
}

#[derive(Clone)]
pub struct PrMeta {
    pub title: String,
    pub body: String,
    pub author: String,
    /// `open`, `closed` or `merged` style, as the forge says it.
    pub state: String,
    pub draft: bool,
    pub merged: bool,
    pub base_ref: String,
    pub head_ref: String,
    pub head_sha: String,
    pub url: String,
    /// GitLab: the diff version's (base, start, head) commits, which inline comments name.
    pub diff_refs: Option<(String, String, String)>,
}

impl PrMeta {
    pub fn is_open(&self) -> bool {
        self.state == "open" || self.state == "opened"
    }
}

pub fn meta(r: &PrRef) -> Result<PrMeta, String> {
    if r.forge == Forge::Gitlab {
        return gitlab::meta(r);
    }
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
        diff_refs: None,
    })
}

/// The request's unified diff at its current head.
pub fn diff(r: &PrRef) -> Result<String, String> {
    if r.forge == Forge::Gitlab {
        return gitlab::diff(r);
    }
    let path = format!("repos/{}/pulls/{}", r.slug(), r.number);
    let bytes = gh(&["api", &path, "-H", "Accept: application/vnd.github.v3.diff"], None)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The checkout the review runs in: the repository itself when it already sits on
/// the request's head, else a detached temporary worktree (removed on drop).
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

fn run_git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), crate::engine::one_line(String::from_utf8_lossy(&out.stderr).trim(), 300)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_owned())
}

/// Make `want_sha` (the request's head) available and return the directory to review
/// in, plus the commit actually reviewed (the head ref's tip at fetch time).
pub fn checkout(repo: &crate::repo::Repo, r: &PrRef, want_sha: &str) -> Result<(Checkout, String), String> {
    if repo.head().as_deref() == Some(want_sha) {
        return Ok((Checkout { root: repo.root.clone(), repo_root: repo.root.clone(), temp: false }, want_sha.to_owned()));
    }
    run_git(&repo.root, &["fetch", "--quiet", "origin", &r.head_ref()])?;
    let fetched = run_git(&repo.root, &["rev-parse", "FETCH_HEAD"])?;
    if fetched != want_sha {
        eprintln!("note: the {} moved while fetching; reviewing {}", r.forge.noun(), &fetched[..fetched.len().min(12)]);
    }
    let dir = std::env::temp_dir().join(format!("review-master-{}{}-{}", r.forge.id(), r.number, uuid::Uuid::new_v4()));
    run_git(&repo.root, &["worktree", "add", "--detach", dir.to_str().ok_or("temp dir is not UTF-8")?, &fetched])?;
    let root = dir.canonicalize().map_err(|e| format!("worktree {}: {e}", dir.display()))?;
    Ok((Checkout { root, repo_root: repo.root.clone(), temp: true }, fetched))
}

/// Line comments already on the request, oldest first, replies after their root
/// (`reply_to`). `line` is null for comments whose code has since changed (outdated);
/// `original_line` still says where.
pub fn review_comments(r: &PrRef, meta: &PrMeta) -> Result<Vec<Value>, String> {
    if r.forge == Forge::Gitlab {
        return gitlab::review_comments(r, meta);
    }
    let path = format!("repos/{}/pulls/{}/comments?per_page=100", r.slug(), r.number);
    let out = gh(&["api", &path, "--paginate", "--jq", ".[]"], None)?;
    parse_review_comments(&out).map_err(|e| format!("gh api {path}: {e}"))
}

/// Post `body` as a reply in the thread started by comment `comment`.
pub fn reply(r: &PrRef, comment: u64, body: &str) -> Result<(), String> {
    if r.forge == Forge::Gitlab {
        return gitlab::reply(r, comment, body);
    }
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

/// The logged-in user on the request's forge, for own-request hints in the UI.
pub fn viewer(r: Option<&PrRef>) -> Option<String> {
    if let Some(r) = r.filter(|r| r.forge == Forge::Gitlab) {
        return gitlab::viewer(&r.host);
    }
    let v: Value = serde_json::from_slice(&gh(&["api", "user"], None).ok()?).ok()?;
    v["login"].as_str().map(str::to_owned)
}

/// The open request whose head is `branch`, if any.
fn open_for_branch(origin: &Remote, forge: Forge, branch: &str) -> Option<u64> {
    if forge == Forge::Gitlab {
        return gitlab::mr_for_branch(&origin.host, &origin.path, branch);
    }
    let out = gh(&["pr", "list", "--repo", &origin.path, "--head", branch, "--state", "open", "--json", "number", "--jq", ".[0].number"], None).ok()?;
    String::from_utf8_lossy(&out).trim().parse().ok()
}

fn open_for_sha(origin: &Remote, forge: Forge, sha: &str) -> Option<u64> {
    if forge == Forge::Gitlab {
        return gitlab::mr_for_sha(&origin.host, &origin.path, sha);
    }
    let jq = format!(".[] | select(.headRefOid==\"{sha}\") | .number");
    let out = gh(&["pr", "list", "--repo", &origin.path, "--state", "open", "--limit", "100", "--json", "number,headRefOid", "--jq", &jq], None).ok()?;
    String::from_utf8_lossy(&out).split_whitespace().next()?.parse().ok()
}

/// The open request this checkout corresponds to — by branch name, then by the
/// branch's upstream (scratch worktrees track the real branch under another local
/// name), then by exact head commit — as a reference `parse_ref` accepts.
pub fn resolve_pr(root: &Path, origin: &Remote) -> Option<String> {
    let forge = origin.forge?;
    let mark = |n: u64| match forge {
        Forge::Github => format!("#{n}"),
        Forge::Gitlab => format!("!{n}"),
    };
    let branch = crate::repo::git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD");
    if let Some(b) = &branch {
        if let Some(n) = open_for_branch(origin, forge, b) {
            println!("review-master: resolved open {} {} from branch `{b}`", forge.noun(), mark(n));
            return Some(mark(n));
        }
    }
    if let Some(upstream) = crate::repo::git(root, &["rev-parse", "--abbrev-ref", "@{upstream}"]) {
        if let Some(b) = upstream.split_once('/').map(|(_, b)| b).filter(|b| Some(*b) != branch.as_deref()) {
            if let Some(n) = open_for_branch(origin, forge, b) {
                println!("review-master: resolved open {} {} from upstream `{upstream}`", forge.noun(), mark(n));
                return Some(mark(n));
            }
        }
    }
    let sha = crate::repo::git(root, &["rev-parse", "HEAD"])?;
    let n = open_for_sha(origin, forge, &sha)?;
    println!("review-master: resolved open {} {} from head commit {}", forge.noun(), mark(n), &sha[..sha.len().min(12)]);
    Some(mark(n))
}

/// A worktree reviewed without a request: its diff against the merge base with the
/// default branch, including uncommitted edits.
pub struct LocalChange {
    pub diff_text: String,
    pub head: String,
    pub branch: String,
    pub dirty: bool,
}

pub fn local_change(root: &Path) -> Result<LocalChange, String> {
    let head = run_git(root, &["rev-parse", "HEAD"]).map_err(|_| format!("{} is not a git checkout", root.display()))?;
    let branch = run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "detached".to_owned());
    let default = ["refs/remotes/origin/HEAD", "origin/main", "origin/master", "main", "master"]
        .iter()
        .find_map(|r| run_git(root, &["rev-parse", "--verify", "--quiet", r]).ok())
        .ok_or("cannot find the default branch (origin/HEAD, main or master); review a pull/merge request instead")?;
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

    fn remote(host: &str, path: &str, forge: Forge) -> Remote {
        Remote { host: host.into(), path: path.into(), forge: Some(forge) }
    }

    #[test]
    fn remote_urls_in_every_form() {
        for (url, host, path) in [
            ("https://github.com/acme/widgets.git", "github.com", "acme/widgets"),
            ("git@github.com:acme/widgets.git", "github.com", "acme/widgets"),
            ("ssh://git@gitlab.example.com:2222/group/sub/proj.git", "gitlab.example.com", "group/sub/proj"),
            ("https://user@Git.Example.LV/group/proj/", "git.example.lv", "group/proj"),
            ("gitlab.com:group/proj", "gitlab.com", "group/proj"),
        ] {
            assert_eq!(parse_remote_url(url), Some((host.to_owned(), path.to_owned())), "{url}");
        }
        assert_eq!(parse_remote_url("/srv/repos/local.git"), None);
    }

    #[test]
    fn references_parse_in_every_supported_form() {
        let gh = remote("github.com", "acme/widgets", Forge::Github);
        let gl = remote("git.example.lv", "group/sub/proj", Forge::Gitlab);
        let cases = [
            ("https://github.com/acme/widgets/pull/176/files", Some(&gh), "github.com", Forge::Github, "acme", "widgets", 176),
            ("acme/widgets#176", Some(&gh), "github.com", Forge::Github, "acme", "widgets", 176),
            ("#176", Some(&gh), "github.com", Forge::Github, "acme", "widgets", 176),
            ("176", Some(&gh), "github.com", Forge::Github, "acme", "widgets", 176),
            ("https://git.example.lv/group/sub/proj/-/merge_requests/12/diffs", None, "git.example.lv", Forge::Gitlab, "group/sub", "proj", 12),
            ("!12", Some(&gl), "git.example.lv", Forge::Gitlab, "group/sub", "proj", 12),
            ("12", Some(&gl), "git.example.lv", Forge::Gitlab, "group/sub", "proj", 12),
            ("other/proj!3", Some(&gl), "git.example.lv", Forge::Gitlab, "other", "proj", 3),
            ("group/proj!3", Some(&gh), "gitlab.com", Forge::Gitlab, "group", "proj", 3),
        ];
        for (raw, origin, host, forge, owner, repo, number) in cases {
            let r = parse_ref(raw, origin, None).unwrap();
            assert_eq!((r.host.as_str(), r.forge, r.owner.as_str(), r.repo.as_str(), r.number), (host, forge, owner, repo, number), "{raw}");
        }
        assert_eq!(parse_ref("!12", Some(&gl), None).unwrap().label(), "group/sub/proj!12");
        assert!(parse_ref("176", None, None).unwrap_err().contains("origin"));
        let unknown = Remote { host: "code.example.com".into(), path: "a/b".into(), forge: None };
        assert!(parse_ref("5", Some(&unknown), None).unwrap_err().contains("--forge"));
        assert!(parse_ref("https://github.com/acme/widgets", Some(&gh), None).is_err());
        assert!(parse_ref("acme/widgets#x", Some(&gh), None).is_err());
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
