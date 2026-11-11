//! GitLab through its CLI (`glab api`, which holds the login for gitlab.com and
//! self-hosted instances): merge request metadata, the diff (rebuilt as one unified
//! diff from the per-file API), the diff discussions already on the request, replies,
//! and the review itself — every inline comment as a draft note, published together,
//! then an optional approval.

use crate::diff::{Kind, Model, Side};
use crate::pr::{self, PrMeta, PrRef};
use crate::review::{Anchored, Finding};
use serde_json::{json, Map, Value};
use std::process::{Command, Stdio};

fn glab(host: &str, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut all = vec!["api", "--hostname", host];
    all.extend_from_slice(args);
    pr::run_cli("glab", "the GitLab CLI", &all, stdin)
}

fn parse(path: &str, bytes: &[u8]) -> Result<Value, String> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null); // 204 No Content
    }
    serde_json::from_slice(bytes).map_err(|e| format!("glab api {path}: {e}"))
}

fn get(host: &str, path: &str) -> Result<Value, String> {
    parse(path, &glab(host, &[path], None)?)
}

/// Every element of a paginated list. `glab --paginate` prints the pages as one array
/// or (older versions) one array per page; both are flattened.
fn get_all(host: &str, path: &str) -> Result<Vec<Value>, String> {
    let out = glab(host, &["--paginate", path], None)?;
    let mut list = Vec::new();
    for v in serde_json::Deserializer::from_slice(&out).into_iter::<Value>() {
        match v.map_err(|e| format!("glab api {path}: {e}"))? {
            Value::Array(items) => list.extend(items),
            other => list.push(other),
        }
    }
    Ok(list)
}

fn post(host: &str, path: &str, body: &Value) -> Result<Value, String> {
    let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let out = glab(host, &["--method", "POST", "-H", "Content-Type: application/json", "--input", "-", path], Some(&bytes))?;
    parse(path, &out)
}

/// A path segment for the API: everything but unreserved characters percent-encoded,
/// so `group/sub/project` becomes one project id.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn mr_path(r: &PrRef) -> String {
    format!("projects/{}/merge_requests/{}", encode(&r.slug()), r.number)
}

/// Whether the GitLab CLI holds a login for `host` (self-hosted instances are known
/// by that, not by their name).
pub fn logged_in(host: &str) -> bool {
    Command::new("glab")
        .args(["auth", "status", "--hostname", host])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn meta(r: &PrRef) -> Result<PrMeta, String> {
    let path = mr_path(r);
    let v = get(&r.host, &path)?;
    let s = |field: &Value| field.as_str().unwrap_or_default().to_owned();
    let refs = &v["diff_refs"];
    let diff_refs = match (refs["base_sha"].as_str(), refs["start_sha"].as_str(), refs["head_sha"].as_str()) {
        (Some(b), Some(s), Some(h)) => Some((b.to_owned(), s.to_owned(), h.to_owned())),
        _ => None,
    };
    // The head of the latest diff version: what the diff and inline comments refer to.
    let head_sha = diff_refs.as_ref().map(|(_, _, h)| h.clone()).unwrap_or_else(|| s(&v["sha"]));
    if head_sha.is_empty() {
        return Err(format!("glab api {path}: no head commit in the response"));
    }
    let state = match v["state"].as_str().unwrap_or_default() {
        "opened" => "open".to_owned(),
        other => other.to_owned(),
    };
    Ok(PrMeta {
        title: s(&v["title"]),
        body: s(&v["description"]),
        author: s(&v["author"]["username"]),
        merged: state == "merged",
        state,
        draft: v["draft"].as_bool().or(v["work_in_progress"].as_bool()).unwrap_or(false),
        base_ref: s(&v["target_branch"]),
        head_ref: s(&v["source_branch"]),
        head_sha,
        url: s(&v["web_url"]),
        diff_refs,
    })
}

/// The merge request's diff, per file from the API, as one git-style unified diff.
pub fn diff(r: &PrRef) -> Result<String, String> {
    let path = format!("{}/diffs?per_page=100", mr_path(r));
    let files = match get_all(&r.host, &path) {
        Ok(files) => files,
        // GitLab before 15.7 has only the (since deprecated) changes endpoint.
        Err(e) if e.contains("404") => get(&r.host, &format!("{}/changes", mr_path(r)))?["changes"].as_array().cloned().unwrap_or_default(),
        Err(e) => return Err(e),
    };
    Ok(unified(&files))
}

fn unified(files: &[Value]) -> String {
    let mut out = String::new();
    for f in files {
        let (Some(old), Some(new)) = (f["old_path"].as_str(), f["new_path"].as_str()) else { continue };
        let flag = |k: &str| f[k].as_bool().unwrap_or(false);
        out.push_str(&format!("diff --git a/{old} b/{new}\n"));
        if flag("new_file") {
            out.push_str(&format!("new file mode {}\n", f["b_mode"].as_str().unwrap_or("100644")));
        }
        if flag("deleted_file") {
            out.push_str(&format!("deleted file mode {}\n", f["a_mode"].as_str().unwrap_or("100644")));
        }
        if flag("renamed_file") {
            out.push_str(&format!("rename from {old}\nrename to {new}\n"));
        }
        let a = if flag("new_file") { "/dev/null".to_owned() } else { format!("a/{old}") };
        let b = if flag("deleted_file") { "/dev/null".to_owned() } else { format!("b/{new}") };
        out.push_str(&format!("--- {a}\n+++ {b}\n"));
        let text = f["diff"].as_str().unwrap_or("");
        out.push_str(text);
        if !text.is_empty() && !text.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Diff discussions as review comments (the shape `pr::review_comments` returns). A
/// note whose position still points at the current diff version is current; GitLab
/// moves positions forward on new pushes when the line did not change, so one left on
/// an older version is outdated.
fn comments(discussions: &[Value], head: &str, url: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for d in discussions {
        let notes: Vec<&Value> = d["notes"].as_array().into_iter().flatten().filter(|n| !n["system"].as_bool().unwrap_or(false)).collect();
        let Some(first) = notes.first() else { continue };
        let pos = &first["position"];
        if pos.is_null() || pos["position_type"].as_str().is_some_and(|t| t != "text") {
            continue; // a general comment, or one on an image/file
        }
        let (side, line, path) = match pos["new_line"].as_u64() {
            Some(n) => ("RIGHT", n, &pos["new_path"]),
            None => ("LEFT", pos["old_line"].as_u64().unwrap_or(0), &pos["old_path"]),
        };
        let range_start = &pos["line_range"]["start"];
        let start = range_start[if side == "RIGHT" { "new_line" } else { "old_line" }].as_u64().filter(|&s| s < line);
        let current = pos["head_sha"].as_str() == Some(head);
        let root = first["id"].clone();
        for (i, n) in notes.iter().enumerate() {
            out.push(json!({
                "id": n["id"],
                "reply_to": if i == 0 { Value::Null } else { root.clone() },
                "path": path,
                "line": if current { json!(line) } else { Value::Null },
                "original_line": line,
                "start": if current { json!(start) } else { Value::Null },
                "side": side,
                "author": n["author"]["username"],
                "body": n["body"],
                "created": n["created_at"],
                "url": format!("{url}#note_{}", n["id"]),
            }));
        }
    }
    out
}

pub fn review_comments(r: &PrRef, meta: &PrMeta) -> Result<Vec<Value>, String> {
    let discussions = get_all(&r.host, &format!("{}/discussions?per_page=100", mr_path(r)))?;
    Ok(comments(&discussions, &meta.head_sha, &meta.url))
}

/// Reply in the discussion that note `note` belongs to.
pub fn reply(r: &PrRef, note: u64, body: &str) -> Result<(), String> {
    let discussions = get_all(&r.host, &format!("{}/discussions?per_page=100", mr_path(r)))?;
    let id = discussions
        .iter()
        .find(|d| d["notes"].as_array().into_iter().flatten().any(|n| n["id"].as_u64() == Some(note)))
        .and_then(|d| d["id"].as_str())
        .ok_or_else(|| format!("no discussion holds note {note}"))?;
    post(&r.host, &format!("{}/discussions/{id}/notes", mr_path(r)), &json!({ "body": body })).map(|_| ())
}

pub fn viewer(host: &str) -> Option<String> {
    get(host, "user").ok()?["username"].as_str().map(str::to_owned)
}

pub fn mr_for_branch(host: &str, project: &str, branch: &str) -> Option<u64> {
    let path = format!("projects/{}/merge_requests?state=opened&source_branch={}", encode(project), encode(branch));
    get(host, &path).ok()?.as_array()?.first()?["iid"].as_u64()
}

pub fn mr_for_sha(host: &str, project: &str, sha: &str) -> Option<u64> {
    let path = format!("projects/{}/merge_requests?state=opened&per_page=100", encode(project));
    get(host, &path).ok()?.as_array()?.iter().find(|m| m["sha"].as_str() == Some(sha))?["iid"].as_u64()
}

/// The note position for an anchored comment. GitLab wants both line numbers for an
/// unchanged line, so they are looked up in the diff.
fn position(model: &Model, a: &Anchored, refs: &(String, String, String)) -> Option<Value> {
    let file = model.file(&a.finding.path)?;
    let line = file.hunks.iter().flat_map(|h| &h.lines).find(|l| match a.side {
        Side::Right => l.new == Some(a.line),
        Side::Left => l.old == Some(a.line) && l.kind != Kind::Add,
    })?;
    let mut p = Map::new();
    p.insert("position_type".into(), "text".into());
    p.insert("base_sha".into(), refs.0.clone().into());
    p.insert("start_sha".into(), refs.1.clone().into());
    p.insert("head_sha".into(), refs.2.clone().into());
    p.insert("old_path".into(), file.old_path.clone().unwrap_or_else(|| file.path.clone()).into());
    p.insert("new_path".into(), file.path.clone().into());
    if let Some(old) = line.old {
        p.insert("old_line".into(), old.into());
    }
    if let Some(new) = line.new {
        p.insert("new_line".into(), new.into());
    }
    Some(Value::Object(p))
}

/// Inline notes anchor on one line; a range is named in the note.
fn note(a: &Anchored) -> String {
    let body = crate::post::comment_body(&a.finding);
    match a.start {
        Some(s) => format!("*Lines {s}–{}:*\n\n{body}", a.line),
        None => body,
    }
}

/// Post the review: each inline comment as a draft note, the summary (with whatever
/// could not be anchored) as a general one, all published at once, then the approval
/// if asked. Returns the merge request URL and, when only the approval failed, why.
pub fn submit(
    r: &PrRef,
    meta: &PrMeta,
    model: &Model,
    approve: bool,
    summary: &str,
    inline: &[Anchored],
    fallback: &[(Finding, String)],
) -> Result<(String, Option<String>), String> {
    let refs = meta.diff_refs.as_ref().ok_or("GitLab gave no diff_refs for this merge request; cannot place inline comments")?;
    let drafts = format!("{}/draft_notes", mr_path(r));
    let mut fallback = fallback.to_vec();
    let mut drafted = 0usize;
    for a in inline {
        let posted = match position(model, a, refs) {
            Some(p) => post(&r.host, &drafts, &json!({ "note": note(a), "position": p })).map(|_| ()),
            None => Err(format!("line {} of `{}` is not in the diff", a.line, a.finding.path)),
        };
        match posted {
            Ok(()) => drafted += 1,
            Err(e) => fallback.push((a.finding.clone(), format!("GitLab refused the position: {}", crate::engine::one_line(&e, 200)))),
        }
    }
    let body = crate::post::review_body(summary, &fallback);
    if !body.is_empty() {
        post(&r.host, &drafts, &json!({ "note": body }))?;
        drafted += 1;
    }
    if drafted > 0 {
        post(&r.host, &format!("{drafts}/bulk_publish"), &json!({}))
            .map_err(|e| format!("{e}; the comments are saved as your pending review on GitLab: publish or discard them there"))?;
    }
    let warning = approve
        .then(|| post(&r.host, &format!("{}/approve", mr_path(r)), &json!({ "sha": refs.2 })).err())
        .flatten()
        .map(|e| format!("the review was published, but approving failed: {}", crate::engine::one_line(&e, 300)));
    Ok((meta.url.clone(), warning))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_file_diffs_become_one_unified_diff() {
        let files = json!([
            { "old_path": "src/a.rs", "new_path": "src/a.rs", "diff": "@@ -1,2 +1,2 @@\n a\n-b\n+c\n" },
            { "old_path": "new.txt", "new_path": "new.txt", "new_file": true, "b_mode": "100644", "diff": "@@ -0,0 +1 @@\n+hi" },
            { "old_path": "gone.txt", "new_path": "gone.txt", "deleted_file": true, "a_mode": "100644", "diff": "@@ -1 +0,0 @@\n-bye\n" },
            { "old_path": "old/name.rs", "new_path": "new/name.rs", "renamed_file": true, "diff": "" },
            { "old_path": "big.bin", "new_path": "big.bin", "diff": "", "too_large": true }
        ]);
        let model = Model::parse(&unified(files.as_array().unwrap()));
        let got: Vec<(&str, &str, Option<&str>, u32, u32)> =
            model.files.iter().map(|f| (f.path.as_str(), f.status.api(), f.old_path.as_deref(), f.additions, f.deletions)).collect();
        assert_eq!(
            got,
            [
                ("src/a.rs", "modified", None, 1, 1),
                ("new.txt", "added", None, 1, 0),
                ("gone.txt", "deleted", None, 0, 1),
                ("new/name.rs", "renamed", Some("old/name.rs"), 0, 0),
                ("big.bin", "modified", None, 0, 0),
            ]
        );
    }

    #[test]
    fn discussions_become_threaded_review_comments() {
        let pos = |head: &str, old: Value, new: Value| {
            json!({ "position_type": "text", "head_sha": head, "old_path": "a.rs", "new_path": "a.rs", "old_line": old, "new_line": new,
                    "line_range": { "start": { "new_line": 10 }, "end": { "new_line": 12 } } })
        };
        let note = |id: u64, who: &str, position: Value| json!({ "id": id, "body": "b", "author": { "username": who }, "created_at": "t", "position": position, "system": false });
        let discussions = json!([
            { "id": "d1", "notes": [note(1, "alice", pos("H", json!(null), json!(12))), note(2, "bob", Value::Null),
                                    { "id": 3, "system": true, "body": "changed this line", "author": { "username": "bob" } }] },
            { "id": "d2", "notes": [note(4, "carol", pos("OLD", json!(7), json!(null)))] },
            { "id": "d3", "individual_note": true, "notes": [note(5, "dave", Value::Null)] }
        ]);
        let list = comments(discussions.as_array().unwrap(), "H", "https://gl/x/-/merge_requests/1");
        let got: Vec<_> = list
            .iter()
            .map(|c| (c["id"].as_u64().unwrap(), c["reply_to"].as_u64(), c["side"].as_str().unwrap(), c["line"].as_u64(), c["start"].as_u64(), c["author"].as_str().unwrap()))
            .collect();
        assert_eq!(
            got,
            [
                (1, None, "RIGHT", Some(12), Some(10), "alice"),
                (2, Some(1), "RIGHT", Some(12), Some(10), "bob"), // system notes are left out
                (4, None, "LEFT", None, None, "carol"),           // outdated: on an older version
            ]
        );
        assert_eq!(list[2]["original_line"], 7);
        assert_eq!(list[0]["url"], "https://gl/x/-/merge_requests/1#note_1");
    }

    #[test]
    fn positions_carry_both_lines_of_unchanged_code() {
        let model = Model::parse("diff --git a/old.rs b/a.rs\nrename from old.rs\nrename to a.rs\n--- a/old.rs\n+++ b/a.rs\n@@ -5,3 +5,3 @@\n ctx\n-gone\n+added\n");
        let refs = ("B".to_owned(), "S".to_owned(), "H".to_owned());
        let at = |side, line| Anchored {
            finding: Finding { path: "a.rs".into(), line, start_line: None, side: None, severity: "nit".into(), title: None, body: "x".into() },
            side,
            start: None,
            line,
        };
        let p = |a: Anchored| position(&model, &a, &refs).map(|v| (v["old_line"].as_u64(), v["new_line"].as_u64(), v["old_path"].as_str().map(str::to_owned)));
        assert_eq!(p(at(Side::Right, 5)), Some((Some(5), Some(5), Some("old.rs".to_owned()))));
        assert_eq!(p(at(Side::Right, 6)), Some((None, Some(6), Some("old.rs".to_owned()))));
        assert_eq!(p(at(Side::Left, 6)), Some((Some(6), None, Some("old.rs".to_owned()))));
        assert_eq!(p(at(Side::Right, 40)), None);
        assert_eq!(encode("group/sub proj"), "group%2Fsub%20proj");
    }
}
