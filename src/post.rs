//! Compose and submit the GitHub review: one POST carrying every line-anchored
//! inline comment at the reviewed head commit. Findings that could not be
//! anchored go into the review body, so nothing is silently dropped.

use crate::pr::{self, PrRef};
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
fn comment_body(f: &Finding) -> String {
    match f.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(title) => format!("**{}: {}**\n\n{}", f.severity, title, f.body),
        None => format!("**{}**\n\n{}", f.severity, f.body),
    }
}

/// The review POST body for `POST /repos/{o}/{r}/pulls/{n}/reviews`.
pub fn payload(sha: &str, event: Event, summary: &str, inline: &[Anchored], fallback: &[(Finding, String)]) -> Value {
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

/// Submit the review; returns its URL.
pub fn submit(r: &PrRef, payload: &Value) -> Result<String, String> {
    let path = format!("repos/{}/pulls/{}/reviews", r.slug(), r.number);
    let bytes = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let out = pr::gh(&["api", &path, "-X", "POST", "--input", "-"], Some(&bytes)).map_err(|e| {
        if e.contains("your own pull request") {
            format!("{e}; use --event comment")
        } else {
            e
        }
    })?;
    let v: Value = serde_json::from_slice(&out).map_err(|e| format!("gh api {path}: {e}"))?;
    Ok(v["html_url"].as_str().map(str::to_owned).unwrap_or_else(|| format!("review id {}", v["id"])))
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
    }

}
