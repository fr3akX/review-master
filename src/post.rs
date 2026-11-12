//! Compose and submit the review. GitHub: one POST carrying every line-anchored inline
//! comment at the reviewed head commit; findings on lines the diff does not show become
//! comments on their whole file, which takes a pending review, a GraphQL call per file
//! comment, then the submission. GitLab: draft notes published together (see
//! `gitlab::submit`). Findings that could not be anchored otherwise go into the review
//! body, so nothing is silently dropped.

use crate::diff::Model;
use crate::pr::{self, Forge, PrMeta, PrRef};
use crate::review::{Anchored, Finding};
use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    Comment,
    Approve,
    RequestChanges,
}

impl Event {
    pub fn api(self) -> &'static str {
        match self {
            Event::Comment => "COMMENT",
            Event::Approve => "APPROVE",
            Event::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

/// `**severity: title**` then the comment body.
pub fn comment_body(f: &Finding) -> String {
    match f.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(title) => format!("**{}: {}**\n\n{}", f.severity, title, f.body),
        None => format!("**{}**\n\n{}", f.severity, f.body),
    }
}

/// The summary, then the findings that could not be anchored, quoted.
pub fn review_body(summary: &str, fallback: &[(Finding, String)]) -> String {
    let mut body = summary.trim().to_owned();
    if !fallback.is_empty() {
        body.push_str("\n\n---\n**Findings that could not be anchored to the diff:**\n");
        for (f, why) in fallback {
            body.push_str(&format!("\n- `{}` ({}; {})\n", f.location(), f.severity, why));
            for line in comment_body(f).lines() {
                body.push_str(&format!("  > {line}\n"));
            }
        }
    }
    body.trim().to_owned()
}

/// The review body; GitHub requires one to request changes.
fn github_body(event: Event, summary: &str, has_comments: bool, fallback: &[(Finding, String)]) -> String {
    let body = review_body(summary, fallback);
    if !body.is_empty() || event != Event::RequestChanges {
        return body;
    }
    if has_comments { "See the inline comments." } else { "Changes requested." }.into()
}

/// A file-level comment: the cited lines, then the finding.
fn file_comment_body(f: &Finding) -> String {
    format!("`{}` (outside the diff)\n\n{}", f.location(), comment_body(f))
}

/// The review POST body for `POST /repos/{o}/{r}/pulls/{n}/reviews`.
pub fn payload(sha: &str, event: Event, summary: &str, inline: &[Anchored], fallback: &[(Finding, String)]) -> Value {
    let body = github_body(event, summary, !inline.is_empty(), fallback);
    let comments: Vec<Value> = inline
        .iter()
        .map(|a| {
            let mut c = json!({
                "path": a.finding.path,
                "line": a.line,
                "side": a.side.api(),
                "body": comment_body(&a.finding),
            });
            if let Some(start) = a.start {
                c["start_line"] = start.into();
                c["start_side"] = a.side.api().into();
            }
            c
        })
        .collect();
    json!({ "commit_id": sha, "event": event.api(), "body": body, "comments": comments })
}

/// Submit the review; returns its URL and, when only a GitLab approval failed, why.
#[allow(clippy::too_many_arguments)]
pub fn submit(
    r: &PrRef,
    meta: &PrMeta,
    model: &Model,
    head: &str,
    event: Event,
    summary: &str,
    inline: &[Anchored],
    fallback: &[(Finding, String)],
) -> Result<(String, Option<String>), String> {
    if r.forge == Forge::Gitlab {
        if event == Event::RequestChanges {
            return Err("GitLab has no request-changes review through its API; submit a comment review (and leave the approval off)".into());
        }
        return crate::gitlab::submit(r, meta, model, event == Event::Approve, summary, inline, fallback);
    }
    let path = format!("repos/{}/pulls/{}/reviews", r.slug(), r.number);
    // A finding in a file of the diff, on lines it does not show, goes on the whole file.
    let (on_file, rest): (Vec<_>, Vec<_>) = fallback.iter().cloned().partition(|(f, _)| model.file(&f.path).is_some());
    if on_file.is_empty() {
        let v = post_json(&path, &payload(head, event, summary, inline, fallback))?;
        return Ok((review_url(&v), None));
    }
    // Without an event the review stays pending, so file comments can join it.
    let mut pending = payload(head, event, "", inline, &[]);
    pending.as_object_mut().expect("object").retain(|k, _| k != "event" && k != "body");
    let v = post_json(&path, &pending)?;
    let node = v["node_id"].as_str().ok_or_else(|| format!("gh api {path}: no node_id in the response"))?;
    let mut rest = rest;
    let mut filed = 0usize;
    for (f, _) in on_file {
        match file_thread(node, &f) {
            Ok(()) => filed += 1,
            Err(e) => rest.push((f, format!("GitHub refused a file comment: {}", crate::engine::one_line(&e, 200)))),
        }
    }
    let body = github_body(event, summary, !inline.is_empty() || filed > 0, &rest);
    let v = post_json(&format!("{path}/{}/events", v["id"]), &json!({ "event": event.api(), "body": body }))
        .map_err(|e| format!("{e}; the comments are saved as your pending review on GitHub: submit or discard it there"))?;
    Ok((review_url(&v), None))
}

fn post_json(path: &str, body: &Value) -> Result<Value, String> {
    let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let out = pr::gh(&["api", path, "-X", "POST", "--input", "-"], Some(&bytes)).map_err(|e| {
        if e.contains("your own pull request") {
            format!("{e}; use --event comment")
        } else {
            e
        }
    })?;
    serde_json::from_slice(&out).map_err(|e| format!("gh api {path}: {e}"))
}

fn review_url(v: &Value) -> String {
    v["html_url"].as_str().map(str::to_owned).unwrap_or_else(|| format!("review id {}", v["id"]))
}

/// Add a comment on `f`'s whole file to the pending review `review` (a GraphQL node id).
fn file_thread(review: &str, f: &Finding) -> Result<(), String> {
    const Q: &str = "mutation($review: ID!, $path: String!, $body: String!) { addPullRequestReviewThread(input: \
        {pullRequestReviewId: $review, path: $path, body: $body, subjectType: FILE}) { thread { id } } }";
    let req = json!({ "query": Q, "variables": { "review": review, "path": f.path, "body": file_comment_body(f) } });
    let out = pr::gh(&["api", "graphql", "--input", "-"], Some(&serde_json::to_vec(&req).map_err(|e| e.to_string())?))?;
    let v: Value = serde_json::from_slice(&out).map_err(|e| format!("gh api graphql: {e}"))?;
    match v["data"]["addPullRequestReviewThread"]["thread"]["id"].as_str() {
        Some(_) => Ok(()),
        None => Err(format!("no thread created: {}", v["errors"])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Side;

    fn finding(title: Option<&str>) -> Finding {
        Finding {
            path: "src/a.rs".into(),
            line: 12,
            start_line: None,
            side: None,
            severity: "major".into(),
            title: title.map(str::to_owned),
            body: "off-by-one in the loop bound".into(),
        }
    }

    #[test]
    fn payload_anchors_ranges_and_keeps_unanchored_findings_in_the_body() {
        let inline = [
            Anchored { finding: finding(Some("loop bound")), side: Side::Right, start: Some(10), line: 12 },
            Anchored { finding: finding(None), side: Side::Left, start: None, line: 7 },
        ];
        let fallback = [(finding(None), "`src/a.rs` is not part of the reviewed diff".to_owned())];
        let p = payload("abc123", Event::RequestChanges, "Needs a fix.", &inline, &fallback);
        assert_eq!(p["commit_id"], "abc123");
        assert_eq!(p["event"], "REQUEST_CHANGES");
        let c = &p["comments"];
        assert_eq!(c[0]["path"], "src/a.rs");
        assert_eq!((c[0]["start_line"].as_u64(), c[0]["line"].as_u64()), (Some(10), Some(12)));
        assert_eq!((c[0]["start_side"].as_str(), c[0]["side"].as_str()), (Some("RIGHT"), Some("RIGHT")));
        assert_eq!(c[0]["body"].as_str().unwrap(), "**major: loop bound**\n\noff-by-one in the loop bound");
        // Single-line comments carry no start fields; LEFT stays LEFT.
        assert!(c[1].get("start_line").is_none());
        assert_eq!(c[1]["side"], "LEFT");
        assert!(c[1]["body"].as_str().unwrap().starts_with("**major**\n\n"));
        // The body keeps the summary and quotes what could not be anchored.
        let body = p["body"].as_str().unwrap();
        assert!(body.starts_with("Needs a fix."));
        assert!(body.contains("`src/a.rs:12` (major; `src/a.rs` is not part of the reviewed diff)"));
        assert!(body.contains("> off-by-one in the loop bound"));
        // An empty summary still posts; only request-changes gets a stand-in body.
        assert_eq!(payload("s", Event::RequestChanges, " ", &inline, &[])["body"], "See the inline comments.");
        assert_eq!(payload("s", Event::Comment, "", &inline, &[])["body"], "");
        // A file-level comment names the lines it was about.
        let mut f = finding(Some("loop bound"));
        f.start_line = Some(3);
        assert!(file_comment_body(&f).starts_with("`src/a.rs:3-12` (outside the diff)\n\n**major: loop bound**"));
    }

}
