//! Review-comment plumbing shared by the UI server: what a comment anchors to,
//! normalization/validation of anchors against the diff, and the formatted
//! subject line describing what is being reviewed (for chat context).

use crate::diff::{Diff, Side};
use crate::pr::{PrMeta, PrRef};

/// One inline comment candidate: where it points and what it says.
#[derive(Debug, Clone)]
pub struct Finding {
    pub path: String,
    pub line: u64,
    pub start_line: Option<u64>,
    /// `"LEFT"` for deleted code; anything else means RIGHT.
    pub side: Option<String>,
    pub severity: String,
    pub title: Option<String>,
    pub body: String,
}

impl Finding {
    pub fn location(&self) -> String {
        match self.start_line {
            Some(start) if start != self.line => format!("{}:{}-{}", self.path, start.min(self.line), start.max(self.line)),
            _ => format!("{}:{}", self.path, self.line),
        }
    }
}

/// A finding whose anchor the diff accepts, normalized for the API.
#[derive(Debug)]
pub struct Anchored {
    pub finding: Finding,
    pub side: Side,
    pub start: Option<u64>,
    pub line: u64,
}

fn side_of(f: &Finding) -> Side {
    match f.side.as_deref() {
        Some("LEFT") => Side::Left,
        _ => Side::Right,
    }
}

/// Normalize (drop no-op ranges, swap reversed ones) and check against the diff.
pub fn anchor(diff: &Diff, f: &Finding) -> Result<Anchored, (Finding, String)> {
    let side = side_of(f);
    let start = f.start_line.filter(|&s| s != f.line);
    let (start, line) = match start {
        Some(s) if s > f.line => (Some(f.line), s),
        other => (other, f.line),
    };
    match diff.check(&f.path, side, start, line) {
        Ok(()) => Ok(Anchored { finding: f.clone(), side, start, line }),
        Err(why) => Err((f.clone(), why)),
    }
}

/// What is being reviewed, already formatted: a PR header or a local branch line.
pub struct Subject {
    /// First line(s): identity, author, branches, head commit.
    pub heading: String,
    /// PR description (may be empty).
    pub description: String,
}

impl Subject {
    pub fn pr(r: &PrRef, meta: &PrMeta, sha: &str) -> Subject {
        Subject {
            heading: format!(
                "The reviewer is reviewing {} {} {} of {}: {}\nAuthor {}; merging `{}` into `{}`; head commit {}.\n",
                r.forge.name(),
                r.forge.noun(),
                r.number_label(),
                r.slug(),
                meta.title,
                meta.author,
                meta.head_ref,
                meta.base_ref,
                &sha[..sha.len().min(12)],
            ),
            description: meta.body.clone(),
        }
    }

    pub fn local(branch: &str, sha: &str, dirty: bool) -> Subject {
        Subject {
            heading: format!(
                "The reviewer is reviewing the local change on branch `{branch}` at commit {}{} (diffed against its merge base with the default branch).\n",
                &sha[..sha.len().min(12)],
                if dirty { ", plus uncommitted edits" } else { "" },
            ),
            description: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(path: &str, start: Option<u64>, line: u64, side: Option<&str>) -> Finding {
        Finding {
            path: path.into(),
            line,
            start_line: start,
            side: side.map(str::to_owned),
            severity: "minor".into(),
            title: None,
            body: "b".into(),
        }
    }

    #[test]
    fn validation_normalizes_ranges_and_reports_why_anchors_fail() {
        let diff = crate::diff::Model::parse(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,3 @@\n context1\n+added2\n context3\n",
        )
        .anchors();
        // Reversed range is swapped, not rejected.
        let a = anchor(&diff, &finding("f.rs", Some(3), 1, None)).unwrap();
        assert_eq!((a.start, a.line), (Some(1), 3));
        // A degenerate range collapses to a single line.
        let a = anchor(&diff, &finding("f.rs", Some(2), 2, None)).unwrap();
        assert_eq!((a.start, a.line), (None, 2));
        let (_, why) = anchor(&diff, &finding("f.rs", None, 9, None)).unwrap_err();
        assert!(why.contains("not shown"));
        let (_, why) = anchor(&diff, &finding("g.rs", None, 1, None)).unwrap_err();
        assert!(why.contains("not part of the reviewed diff"));
        // LEFT uses old numbering: old line 2 is context3.
        assert!(anchor(&diff, &finding("f.rs", None, 2, Some("LEFT"))).is_ok());
    }
}
