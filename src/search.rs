//! Text search across the repository: whole-word matches are the navigation
//! fallback for files without a language server (YAML, shell, configs, …);
//! substring matches back the viewer's search box.

use serde::Serialize;
use std::fs;
use std::path::Path;

const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Serialize)]
pub struct Hit {
    pub path: String,
    /// 1-based.
    pub line: usize,
    pub preview: String,
}

#[derive(Serialize)]
pub struct Results {
    pub hits: Vec<Hit>,
    pub truncated: bool,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Occurrences of `word` not embedded in a longer identifier.
fn contains_word(line: &str, word: &str) -> bool {
    let bytes = line.as_bytes();
    line.match_indices(word).any(|(i, m)| {
        let before = i.checked_sub(1).map(|j| bytes[j]);
        let after = bytes.get(i + m.len()).copied();
        !before.is_some_and(is_word_byte) && !after.is_some_and(is_word_byte)
    })
}

pub enum Query {
    /// A whole identifier, case-sensitive.
    Word(String),
    /// A substring; case-insensitive unless it contains an uppercase letter (smart case).
    Text(String),
}

impl Query {
    fn ignore_case(&self) -> bool {
        matches!(self, Query::Text(q) if !q.chars().any(char::is_uppercase))
    }

    /// `needle` is already lowercased when the search ignores case.
    fn matches(&self, line: &str, needle: &str) -> bool {
        match self {
            Query::Word(_) => contains_word(line, needle),
            Query::Text(_) if self.ignore_case() => line.to_lowercase().contains(needle),
            Query::Text(_) => line.contains(needle),
        }
    }
}

/// Search tracked-looking files (respecting .gitignore, skipping `.git`,
/// binaries and files over 1 MiB), stopping after `limit` hits.
pub fn search(root: &Path, query: &Query, limit: usize) -> Results {
    let needle = match query {
        Query::Word(w) => w.clone(),
        Query::Text(q) if query.ignore_case() => q.to_lowercase(),
        Query::Text(q) => q.clone(),
    };
    let mut hits = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| !crate::repo::is_tool_dir(e.file_name()))
        .build();
    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if entry.metadata().map_or(true, |m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else { continue };
        if bytes.iter().take(8192).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let candidate = if query.ignore_case() { text.to_lowercase().contains(&needle) } else { text.contains(&needle) };
        if !candidate {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path()).to_string_lossy().into_owned();
        for (i, line) in text.lines().enumerate() {
            if query.matches(line, &needle) {
                if hits.len() == limit {
                    return Results { hits, truncated: true };
                }
                hits.push(Hit { path: rel.clone(), line: i + 1, preview: line.trim().chars().take(200).collect() });
            }
        }
    }
    hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Results { hits, truncated: false }
}

#[cfg(test)]
mod tests {
    use super::{contains_word, Query};

    #[test]
    fn matches_whole_identifiers_only() {
        assert!(contains_word("let user = load(user_id);", "user"));
        assert!(contains_word("x.user)", "user"));
        assert!(!contains_word("let users = user_id;", "user"));
        assert!(!contains_word("$user", "user"));
        assert!(contains_word("user", "user"));
    }

    #[test]
    fn text_queries_use_smart_case() {
        let lower = Query::Text("user".into());
        assert!(lower.matches("let User = 1;", "user"));
        assert!(lower.matches("load_users()", "user"));
        let upper = Query::Text("User".into());
        assert!(upper.matches("struct User;", "User"));
        assert!(!upper.matches("let user = 1;", "User"));
    }
}
