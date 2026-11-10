//! Read-only repository tools for review-master's own agent loops (native Anthropic and
//! OpenAI-compatible): numbered file reads, regex search, glob and directory listing.
//! Everything is confined to the canonical repository root, skips tool state
//! directories and ignored files, and returns bounded plain text. Nothing here
//! executes code or touches the network.

use crate::repo::is_tool_dir;
use crate::verify;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

/// Upper bound on any tool's text output.
const MAX_OUTPUT_BYTES: usize = 48 * 1024;
const MAX_READ_FILE_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_READ_LINES: usize = 400;
const MAX_READ_LINES: usize = 2000;
const MAX_LINE_BYTES: usize = 2000;
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;
const MAX_GREP_HITS: usize = 200;
const MAX_PREVIEW_CHARS: usize = 300;
const MAX_GLOB_RESULTS: usize = 500;
const MAX_LIST_ENTRIES: usize = 1000;
const MAX_PATTERN_CHARS: usize = 1000;
const BINARY_SNIFF_BYTES: usize = 8192;

type Args = serde_json::Map<String, Value>;

pub struct Tools {
    /// Canonical repository root.
    root: PathBuf,
}

impl Tools {
    pub fn new(root: &Path) -> Result<Tools, String> {
        let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        Ok(Tools { root })
    }

    /// Definitions for `_read`, `_grep`, `_glob` and `_list` as `{ name, description,
    /// input_schema }` (the Anthropic shape; OpenAI-compatible engines convert them).
    pub fn definitions() -> Vec<Value> {
        vec![
            json!({
                "name": "_read",
                "description": "Read a text file from the repository with 1-based line numbers, one `N<TAB>text` per line. Returns at most `limit` lines (default 400, max 2000) and about 48 KiB; lines over 2000 bytes are cut. When more remains, the last line says which start_line to continue from. Paths are relative to the repository root.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Repository-relative file path." },
                        "start_line": { "type": "integer", "minimum": 1, "description": "First line to show (1-based, default 1)." },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_READ_LINES, "description": "Maximum number of lines (default 400)." }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }),
            json!({
                "name": "_grep",
                "description": "Search file contents with a regular expression (Rust regex syntax, case-sensitive; prefix `(?i)` to ignore case). Matches line by line and returns `path:line: text`, at most 200 hits. Skips ignored files, tool directories, binary files and files over 1 MiB.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regular expression matched against each line." },
                        "path": { "type": "string", "description": "Repository-relative file or directory to search (default: whole repository)." }
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }
            }),
            json!({
                "name": "_glob",
                "description": "Find repository files by glob pattern relative to the repository root: `*` stays within one directory, `**` crosses directories, `{a,b}` alternates. A pattern without `/` matches file names at any depth. Returns up to 500 file paths; ignored files and tool directories are skipped.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob such as `src/**/*.rs` or `*.toml`." }
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }
            }),
            json!({
                "name": "_list",
                "description": "List one repository directory: subdirectories end with `/`, files show their size, symlinks are marked `@`. Ignored entries and tool directories are omitted.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Repository-relative directory (default: repository root)." }
                    },
                    "additionalProperties": false
                }
            }),
        ]
    }

    /// Run one tool. Errors are tool errors meant for the model (bad input, missing
    /// file, path outside the repository), never panics.
    pub async fn call(&self, name: &str, input: &Value) -> Result<String, String> {
        let root = self.root.clone();
        let tool = name.to_owned();
        let input = input.clone();
        tokio::task::spawn_blocking(move || run(&root, &tool, &input))
            .await
            .map_err(|e| format!("{name} failed: {e}"))?
    }
}

fn run(root: &Path, name: &str, input: &Value) -> Result<String, String> {
    let args = input.as_object().ok_or("tool input must be a JSON object")?;
    match name {
        "_read" => read(root, required(args, "path")?, uint_arg(args, "start_line")?, uint_arg(args, "limit")?),
        "_grep" => grep(root, required(args, "pattern")?, str_arg(args, "path")?),
        "_glob" => glob(root, required(args, "pattern")?),
        "_list" => list(root, str_arg(args, "path")?),
        other => Err(format!("unknown tool `{other}`")),
    }
}

fn str_arg<'a>(args: &'a Args, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn required<'a>(args: &'a Args, key: &str) -> Result<&'a str, String> {
    str_arg(args, key)?.ok_or_else(|| format!("missing required string field `{key}`"))
}

fn uint_arg(args: &Args, key: &str) -> Result<Option<usize>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 => Ok(Some(usize::try_from(n).unwrap_or(usize::MAX))),
            _ => Err(format!("`{key}` must be a positive integer")),
        },
    }
}

/// A repository path resolved through `verify::resolve` (canonical, symlink escapes
/// refused) that is not inside a tool state directory.
fn confined(root: &Path, raw: &str) -> Result<(PathBuf, String), String> {
    let (full, shown) = verify::resolve(root, raw).map_err(|e| format!("{}: {e}", raw.trim()))?;
    if Path::new(&shown).components().any(|c| matches!(c, Component::Normal(n) if is_tool_dir(n))) {
        return Err(format!("{shown}: tool state, not project content"));
    }
    Ok((full, shown))
}

fn target(root: &Path, path: Option<&str>) -> Result<(PathBuf, String), String> {
    match path {
        Some(p) if !p.trim().is_empty() && p.trim() != "." => confined(root, p),
        _ => Ok((root.to_path_buf(), ".".into())),
    }
}

/// Walk the repository from its root, so its ignore files apply, restricted to
/// `target` and its ancestors. Symlinks are reported, never followed; ignore files
/// above the root are not consulted.
fn walk(root: &Path, target: &Path, max_depth: Option<usize>) -> ignore::Walk {
    let target = target.to_path_buf();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(max_depth)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |e| {
            !is_tool_dir(e.file_name()) && (e.path().starts_with(&target) || target.starts_with(e.path()))
        });
    builder.build()
}

fn relative(root: &Path, entry: &ignore::DirEntry) -> Option<String> {
    let rel = entry.path().strip_prefix(root).ok()?;
    (!rel.as_os_str().is_empty()).then(|| rel.to_string_lossy().into_owned())
}

/// The real file behind a walked entry: regular files as is, symlinks only when
/// they resolve to a regular file inside the repository.
fn readable(root: &Path, entry: &ignore::DirEntry, rel: &str) -> Option<PathBuf> {
    let kind = entry.file_type()?;
    if kind.is_file() {
        return Some(entry.path().to_path_buf());
    }
    if !kind.is_symlink() {
        return None;
    }
    let (full, _) = confined(root, rel).ok()?;
    full.is_file().then_some(full)
}

fn is_binary(reader: impl Read) -> std::io::Result<bool> {
    let mut head = Vec::with_capacity(BINARY_SNIFF_BYTES);
    reader.take(BINARY_SNIFF_BYTES as u64).read_to_end(&mut head)?;
    Ok(head.contains(&0))
}

/// Decode a line lossily; a line cut at the byte limit drops its split last character.
fn line_text(bytes: &[u8], cut: bool) -> String {
    let bytes = if cut {
        match std::str::from_utf8(bytes) {
            Err(e) if e.error_len().is_none() => &bytes[..e.valid_up_to()],
            _ => bytes,
        }
    } else {
        bytes.strip_suffix(b"\r").unwrap_or(bytes)
    };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Numbered lines of the requested window, within the output budget.
struct Window {
    start: usize,
    /// Exclusive.
    end: usize,
    body: String,
    last: usize,
    full: bool,
}

impl Window {
    fn wants(&self, n: usize) -> bool {
        n >= self.start && n < self.end && !self.full
    }

    fn push(&mut self, n: usize, bytes: &[u8], cut: bool) {
        if !self.wants(n) {
            return;
        }
        let marker = if cut { " …[line truncated]" } else { "" };
        let entry = format!("{n}\t{}{marker}\n", line_text(bytes, cut));
        if self.last != 0 && self.body.len() + entry.len() > MAX_OUTPUT_BYTES {
            self.full = true;
            return;
        }
        self.body.push_str(&entry);
        self.last = n;
    }
}

fn read(root: &Path, raw: &str, start: Option<usize>, limit: Option<usize>) -> Result<String, String> {
    let (full, shown) = confined(root, raw)?;
    let io = |e: std::io::Error| format!("{shown}: {e}");
    let meta = fs::metadata(&full).map_err(io)?;
    if meta.is_dir() {
        return Err(format!("{shown} is a directory; use _list"));
    }
    if !meta.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    if meta.len() > MAX_READ_FILE_BYTES {
        return Err(format!("{shown} is larger than 16 MiB"));
    }
    let mut file = File::open(&full).map_err(io)?;
    if is_binary(&mut file).map_err(io)? {
        return Err(format!("{shown} is a binary file"));
    }
    file.seek(SeekFrom::Start(0)).map_err(io)?;

    let start = start.unwrap_or(1);
    let limit = limit.unwrap_or(DEFAULT_READ_LINES).min(MAX_READ_LINES);
    let mut window = Window { start, end: start.saturating_add(limit), body: String::new(), last: 0, full: false };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut total = 0usize;
    let mut line = Vec::new();
    let mut cut = false;
    let mut open = false;
    loop {
        let (consumed, ended) = {
            let buf = reader.fill_buf().map_err(io)?;
            if buf.is_empty() {
                break;
            }
            let (chunk, consumed, ended) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (&buf[..i], i + 1, true),
                None => (buf, buf.len(), false),
            };
            if window.wants(total + 1) {
                let take = MAX_LINE_BYTES.saturating_sub(line.len()).min(chunk.len());
                line.extend_from_slice(&chunk[..take]);
                cut |= take < chunk.len();
            }
            (consumed, ended)
        };
        reader.consume(consumed);
        open = !ended;
        if ended {
            total += 1;
            window.push(total, &line, cut);
            line.clear();
            cut = false;
        }
    }
    if open {
        total += 1;
        window.push(total, &line, cut);
    }

    if total == 0 {
        return Ok(format!("{shown}: empty file"));
    }
    if start > total {
        return Err(format!("{shown} has {total} lines; start_line {start} is past the end"));
    }
    let last = window.last;
    let mut out = format!("{shown} (lines {start}-{last} of {total})\n");
    out.push_str(&window.body);
    if last < total {
        out.push_str(&format!("[more: continue with start_line={}]\n", last + 1));
    }
    Ok(out)
}

fn build_regex(pattern: &str) -> Result<regex::Regex, String> {
    if pattern.is_empty() {
        return Err("empty grep pattern".into());
    }
    if pattern.chars().count() > MAX_PATTERN_CHARS {
        return Err(format!("grep pattern longer than {MAX_PATTERN_CHARS} characters"));
    }
    regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .dfa_size_limit(4 << 20)
        .nest_limit(64)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))
}

fn preview(line: &str) -> String {
    let line = line.trim();
    let mut chars = line.char_indices();
    match chars.nth(MAX_PREVIEW_CHARS) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_owned(),
    }
}

fn grep(root: &Path, pattern: &str, path: Option<&str>) -> Result<String, String> {
    let regex = build_regex(pattern)?;
    let (dir, shown) = target(root, path)?;
    let mut out = String::new();
    let mut hits = 0usize;
    let mut truncated = false;
    let mut skipped = 0usize;
    'files: for entry in walk(root, &dir, None).filter_map(Result::ok) {
        let Some(rel) = relative(root, &entry) else { continue };
        let Some(file) = readable(root, &entry, &rel) else { continue };
        if fs::metadata(&file).map_or(true, |m| m.len() > MAX_GREP_FILE_BYTES) {
            skipped += 1;
            continue;
        }
        let Ok(bytes) = fs::read(&file) else { continue };
        if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        for (i, line) in text.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            let entry = format!("{rel}:{}: {}\n", i + 1, preview(line));
            if hits == MAX_GREP_HITS || out.len() + entry.len() > MAX_OUTPUT_BYTES {
                truncated = true;
                break 'files;
            }
            out.push_str(&entry);
            hits += 1;
        }
    }
    if hits == 0 {
        out = format!("no matches for `{pattern}` in {shown}\n");
    }
    if truncated {
        out.push_str(&format!("[truncated after {hits} matches; narrow the pattern or path]\n"));
    }
    if skipped > 0 {
        out.push_str(&format!("[{skipped} files over 1 MiB not searched]\n"));
    }
    Ok(out)
}

fn build_glob(pattern: &str) -> Result<globset::GlobMatcher, String> {
    let p = pattern.trim();
    if p.is_empty() {
        return Err("empty glob pattern".into());
    }
    if p.chars().count() > MAX_PATTERN_CHARS {
        return Err(format!("glob pattern longer than {MAX_PATTERN_CHARS} characters"));
    }
    if p.starts_with('/') || p.starts_with('\\') || Path::new(p).is_absolute() {
        return Err("glob patterns are relative to the repository root; absolute patterns are not allowed".into());
    }
    let p = p.trim_start_matches("./");
    if p.split(['/', '\\']).any(|segment| segment == "..") {
        return Err("`..` is not allowed in glob patterns".into());
    }
    let mut p = p.to_owned();
    if p.is_empty() || p.ends_with('/') {
        p.push_str("**");
    }
    if !p.contains('/') {
        p.insert_str(0, "**/");
    }
    globset::GlobBuilder::new(&p)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("invalid glob `{pattern}`: {e}"))
}

fn glob(root: &Path, pattern: &str) -> Result<String, String> {
    let matcher = build_glob(pattern)?;
    let mut out = String::new();
    let mut count = 0usize;
    let mut truncated = false;
    for entry in walk(root, root, None).filter_map(Result::ok) {
        let Some(rel) = relative(root, &entry) else { continue };
        if !matcher.is_match(&rel) || readable(root, &entry, &rel).is_none() {
            continue;
        }
        if count == MAX_GLOB_RESULTS || out.len() + rel.len() + 1 > MAX_OUTPUT_BYTES {
            truncated = true;
            break;
        }
        out.push_str(&rel);
        out.push('\n');
        count += 1;
    }
    if count == 0 {
        return Ok(format!("no files match `{pattern}`\n"));
    }
    if truncated {
        out.push_str(&format!("[truncated after {count} files; narrow the pattern]\n"));
    }
    Ok(out)
}

fn size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KiB", b as f64 / KIB),
        b => format!("{:.1} MiB", b as f64 / (KIB * KIB)),
    }
}

fn list(root: &Path, path: Option<&str>) -> Result<String, String> {
    let (dir, shown) = target(root, path)?;
    if !dir.is_dir() {
        return Err(format!("{shown} is not a directory; use _read"));
    }
    let depth = dir.strip_prefix(root).map_or(0, |r| r.components().count()) + 1;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut bytes = shown.len() + 2;
    let mut truncated = false;
    for entry in walk(root, &dir, Some(depth)).filter_map(Result::ok) {
        if entry.depth() != depth || !entry.path().starts_with(&dir) {
            continue;
        }
        if dirs.len() + files.len() >= MAX_LIST_ENTRIES {
            truncated = true;
            break;
        }
        let name = entry.file_name().to_string_lossy();
        let Some(kind) = entry.file_type() else { continue };
        let row = if kind.is_dir() {
            format!("{name}/")
        } else if kind.is_symlink() {
            let link = relative(root, &entry).and_then(|rel| confined(root, &rel).ok());
            match link {
                Some((full, to)) if full.is_dir() => format!("{name}@ -> {to}/"),
                Some((_, to)) => format!("{name}@ -> {to}"),
                None => format!("{name}@ (outside the repository or missing)"),
            }
        } else {
            let len = entry.metadata().map_or(0, |m| m.len());
            format!("{name}  {}", size(len))
        };
        if bytes + row.len() + 100 > MAX_OUTPUT_BYTES {
            truncated = true;
            break;
        }
        bytes += row.len() + 1;
        if kind.is_dir() { dirs.push(row); } else { files.push(row); }
    }
    let mut out = format!("{}/\n", shown.trim_end_matches('/'));
    if dirs.is_empty() && files.is_empty() && !truncated {
        out.push_str("(no entries; the directory is empty or ignored)\n");
    }
    for line in dirs.iter().chain(&files) {
        out.push_str(line);
        out.push('\n');
    }
    if truncated {
        out.push_str(&format!("[truncated after {} entries; list a narrower path]\n", dirs.len() + files.len()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRepo {
        root: PathBuf,
        outside: PathBuf,
    }

    impl TempRepo {
        fn new(tag: &str) -> TempRepo {
            let base = std::env::temp_dir().join(format!("review-master-test-{tag}-{}", std::process::id()));
            let outside = base.with_extension("outside");
            let _ = fs::remove_dir_all(&base);
            let _ = fs::remove_file(&outside);
            fs::create_dir_all(base.join("src")).unwrap();
            fs::create_dir_all(base.join("ignored")).unwrap();
            fs::create_dir_all(base.join(".git")).unwrap();
            fs::write(base.join("src/main.rs"), "fn main() {\n    run();\n}\n").unwrap();
            fs::write(base.join("ignored/generated.rs"), "fn run() {}\n").unwrap();
            fs::write(base.join(".git/config"), "run = true\n").unwrap();
            fs::write(base.join(".gitignore"), "ignored/\n").unwrap();
            fs::write(&outside, "SECRET_OUTSIDE_VALUE\n").unwrap();
            TempRepo { root: base.canonicalize().unwrap(), outside }
        }

        fn call(&self, name: &str, input: Value) -> Result<String, String> {
            run(&self.root, name, &input)
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_file(&self.outside);
        }
    }


    #[test]
    fn paths_stay_inside_the_repository() {
        let repo = TempRepo::new("tools-confine");
        assert!(repo.call("_read", json!({ "path": "../outside" })).is_err());
        assert!(repo.call("_read", json!({ "path": repo.outside.to_str().unwrap() })).is_err());
        assert!(repo.call("_read", json!({ "path": "src/../../x" })).is_err());
        assert!(repo.call("_read", json!({ "path": ".git/config" })).is_err());
        assert!(repo.call("_list", json!({ "path": ".git" })).is_err());
        assert!(repo.call("_grep", json!({ "pattern": "run", "path": ".git" })).is_err());
        let inside = repo.root.join("src/main.rs");
        assert!(repo.call("_read", json!({ "path": inside.to_str().unwrap() })).unwrap().contains("2\t    run();"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_repository_are_refused() {
        let repo = TempRepo::new("tools-symlink");
        std::os::unix::fs::symlink(&repo.outside, repo.root.join("src/leak.rs")).unwrap();
        std::os::unix::fs::symlink(repo.outside.parent().unwrap(), repo.root.join("escape")).unwrap();
        std::os::unix::fs::symlink(repo.root.join("src/main.rs"), repo.root.join("src/alias.rs")).unwrap();

        assert!(repo.call("_read", json!({ "path": "src/leak.rs" })).is_err());
        assert!(repo.call("_list", json!({ "path": "escape" })).is_err());
        let found = repo.call("_grep", json!({ "pattern": "SECRET_OUTSIDE" })).unwrap();
        assert!(found.starts_with("no matches"), "{found}");
        let globbed = repo.call("_glob", json!({ "pattern": "*.rs" })).unwrap();
        assert!(!globbed.contains("leak.rs"), "{globbed}");
        assert!(globbed.contains("src/alias.rs"), "{globbed}");
        let listed = repo.call("_list", json!({ "path": "src" })).unwrap();
        assert!(listed.contains("leak.rs@ (outside"), "{listed}");
        assert!(listed.contains("alias.rs@ -> src/main.rs"), "{listed}");
        assert!(repo.call("_read", json!({ "path": "src/alias.rs" })).unwrap().contains("1\tfn main() {"));
    }

    #[test]
    fn reads_numbered_ranges() {
        let repo = TempRepo::new("tools-read");
        let text: String = (1..=10).map(|i| format!("line {i}\r\n")).collect();
        fs::write(repo.root.join("lines.txt"), text).unwrap();

        let part = repo.call("_read", json!({ "path": "lines.txt", "start_line": 3, "limit": 2 })).unwrap();
        assert!(part.starts_with("lines.txt (lines 3-4 of 10)\n"), "{part}");
        assert!(part.contains("3\tline 3\n4\tline 4\n"), "{part}");
        assert!(!part.contains("5\tline 5"), "{part}");
        assert!(part.ends_with("[more: continue with start_line=5]\n"), "{part}");

        let whole = repo.call("_read", json!({ "path": "lines.txt" })).unwrap();
        assert!(whole.contains("10\tline 10\n") && !whole.contains("[more"), "{whole}");

        assert!(repo.call("_read", json!({ "path": "lines.txt", "start_line": 11 })).is_err());
        assert!(repo.call("_read", json!({ "path": "lines.txt", "start_line": 0 })).is_err());
        assert!(repo.call("_read", json!({ "path": "lines.txt", "start_line": "3" })).is_err());
        assert!(repo.call("_read", json!({ "path": 3 })).is_err());
        assert!(repo.call("_read", json!({})).is_err());
        assert!(repo.call("_read", json!("lines.txt")).is_err());
        assert!(repo.call("_read", json!({ "path": "src" })).is_err());
        assert!(repo.call("_shell", json!({})).is_err());
    }

    #[test]
    fn reads_stay_within_the_output_budget() {
        let repo = TempRepo::new("tools-budget");
        let long = "é".repeat(3000);
        fs::write(repo.root.join("wide.txt"), format!("{long}\nnext\n")).unwrap();
        let wide = repo.call("_read", json!({ "path": "wide.txt" })).unwrap();
        assert!(wide.contains(&format!("1\t{} …[line truncated]\n", "é".repeat(MAX_LINE_BYTES / 2))), "{wide}");
        assert!(wide.contains("2\tnext\n"));

        let text: String = (1..=2000).map(|i| format!("{i:0>99}\n")).collect();
        fs::write(repo.root.join("big.txt"), text).unwrap();
        let big = repo.call("_read", json!({ "path": "big.txt", "limit": 2000 })).unwrap();
        assert!(big.len() < MAX_OUTPUT_BYTES + 200, "{}", big.len());
        assert!(big.contains("[more: continue with start_line="), "{big}");
    }

    #[test]
    fn grep_respects_ignores_and_reports_lines() {
        let repo = TempRepo::new("tools-grep");
        fs::write(repo.root.join("src/ünï.txt"), "naïve\ncafé crème\n").unwrap();
        let found = repo.call("_grep", json!({ "pattern": "run" })).unwrap();
        assert_eq!(found, "src/main.rs:2: run();\n");
        let unicode = repo.call("_grep", json!({ "pattern": "caf.", "path": "src" })).unwrap();
        assert_eq!(unicode, "src/ünï.txt:2: café crème\n");
        let scoped = repo.call("_grep", json!({ "pattern": "fn", "path": "src/main.rs" })).unwrap();
        assert_eq!(scoped, "src/main.rs:1: fn main() {\n");
        assert!(repo.call("_grep", json!({ "pattern": "" })).is_err());
        assert!(repo.call("_grep", json!({ "pattern": "(" })).is_err());
        assert!(repo.call("_grep", json!({ "pattern": ["run"] })).is_err());
    }

    #[test]
    fn grep_truncates_hits() {
        let repo = TempRepo::new("tools-grep-cap");
        let text: String = (1..=300).map(|i| format!("hit {i}\n")).collect();
        fs::write(repo.root.join("hits.txt"), text).unwrap();
        let found = repo.call("_grep", json!({ "pattern": "^hit" })).unwrap();
        assert_eq!(found.lines().filter(|l| l.starts_with("hits.txt:")).count(), MAX_GREP_HITS);
        assert!(found.contains("hits.txt:200: hit 200\n"));
        assert!(found.contains(&format!("[truncated after {MAX_GREP_HITS} matches")), "{found}");
    }

    #[test]
    fn glob_respects_ignores_and_rejects_escapes() {
        let repo = TempRepo::new("tools-glob");
        fs::create_dir_all(repo.root.join("src/deep")).unwrap();
        fs::write(repo.root.join("src/deep/mod.rs"), "").unwrap();
        assert_eq!(repo.call("_glob", json!({ "pattern": "*.rs" })).unwrap(), "src/deep/mod.rs\nsrc/main.rs\n");
        assert_eq!(repo.call("_glob", json!({ "pattern": "src/*.rs" })).unwrap(), "src/main.rs\n");
        assert_eq!(repo.call("_glob", json!({ "pattern": "./src/**/*.rs" })).unwrap(), "src/deep/mod.rs\nsrc/main.rs\n");
        assert!(repo.call("_glob", json!({ "pattern": "ignored/*" })).unwrap().starts_with("no files match"));
        assert!(repo.call("_glob", json!({ "pattern": "**/config" })).unwrap().starts_with("no files match"));
        assert!(repo.call("_glob", json!({ "pattern": "../*" })).is_err());
        assert!(repo.call("_glob", json!({ "pattern": "src/../../*" })).is_err());
        assert!(repo.call("_glob", json!({ "pattern": "/etc/*" })).is_err());
        assert!(repo.call("_glob", json!({ "pattern": " " })).is_err());
        assert!(repo.call("_glob", json!({ "pattern": "src/[" })).is_err());
    }

    #[test]
    fn list_skips_tool_and_ignored_entries() {
        let repo = TempRepo::new("tools-list");
        let top = repo.call("_list", json!({})).unwrap();
        assert_eq!(top, "./\nsrc/\n.gitignore  9 B\n");
        let src = repo.call("_list", json!({ "path": "src" })).unwrap();
        assert_eq!(src, "src/\nmain.rs  25 B\n");
        assert!(repo.call("_list", json!({ "path": "src/main.rs" })).is_err());
    }

    #[test]
    fn list_caps_long_names_before_exceeding_the_output_budget() {
        let repo = TempRepo::new("tools-list-budget");
        for n in 0..500 {
            fs::write(repo.root.join("src").join(format!("{n:03}{}", "x".repeat(230))), "").unwrap();
        }
        let listing = repo.call("_list", json!({ "path": "src" })).unwrap();
        assert!(listing.len() <= MAX_OUTPUT_BYTES);
        assert!(listing.contains("[truncated after"));
        assert!(!listing.contains(&format!("499{}", "x".repeat(230))));
    }
}
