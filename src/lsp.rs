//! Minimal Language Server Protocol client for source navigation: hover, go to
//! definition and find references. One server process per language, started
//! on first use with the repository as workspace root.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;

const INIT_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
/// Importing a build downloads the build tool and dependencies on first run.
const BUILD_IMPORT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_REFERENCES: usize = 500;

/// Language → candidate server commands; the first one found on PATH is used.
const DEFAULT_SERVERS: &[(&str, &[&str])] = &[
    ("rust", &["rust-analyzer"]),
    ("python", &["pyright-langserver --stdio", "basedpyright-langserver --stdio", "pylsp", "jedi-language-server"]),
    ("typescript", &["typescript-language-server --stdio"]),
    ("go", &["gopls"]),
    ("c", &["clangd"]),
    ("scala", &["metals"]),
];

pub const LANGUAGES: &[&str] = &["rust", "python", "typescript", "go", "c", "scala"];

/// How to get each language's server, for when none is found.
const INSTALL_HINTS: &[(&str, &str)] = &[
    ("rust", "rustup component add rust-analyzer"),
    ("python", "npm install -g pyright (or pip install pyright)"),
    ("typescript", "npm install -g typescript typescript-language-server"),
    ("go", "go install golang.org/x/tools/gopls@latest"),
    ("c", "install clangd (apt install clangd, brew install llvm)"),
    ("scala", "cs install metals (coursier, needs a JDK)"),
];

fn install_hint(lang: &str) -> &'static str {
    INSTALL_HINTS.iter().find(|(l, _)| *l == lang).map_or("see the README", |(_, h)| *h)
}

/// Server language for a file, plus the document's LSP `languageId`.
pub fn language_of(path: &str) -> Option<(&'static str, &'static str)> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => ("rust", "rust"),
        "py" | "pyi" => ("python", "python"),
        "ts" | "mts" | "cts" => ("typescript", "typescript"),
        "tsx" => ("typescript", "typescriptreact"),
        "js" | "mjs" | "cjs" => ("typescript", "javascript"),
        "jsx" => ("typescript", "javascriptreact"),
        "go" => ("go", "go"),
        "c" | "h" => ("c", "c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => ("c", "cpp"),
        "scala" | "sc" | "sbt" => ("scala", "scala"),
        _ => return None,
    })
}

/// Settings that keep servers from running project code (build scripts,
/// proc macros, `cargo check`) while browsing an untrusted repository.
fn initialization_options(lang: &str) -> Value {
    match lang {
        "rust" => json!({
            "cargo": { "buildScripts": { "enable": false } },
            "procMacro": { "enable": false },
            "checkOnSave": false
        }),
        _ => Value::Null,
    }
}

/// Answer to `window/showMessageRequest`. Metals asks before importing a build, which
/// runs sbt, Mill, Gradle, … on the repository: dismiss unless the user opted in, then
/// pick "Import build" (or the build tool, when Metals asks which one to use).
fn message_choice(params: &Value, import_builds: bool) -> Value {
    if !import_builds {
        return Value::Null;
    }
    let about_builds = params["message"].as_str().is_some_and(|m| m.to_lowercase().contains("build"));
    let tools = ["sbt", "mill", "gradle", "maven", "scala-cli", "bloop", "bazel"];
    let chosen = params["actions"].as_array().into_iter().flatten().find(|a| {
        let title = a["title"].as_str().unwrap_or("").to_lowercase();
        title.starts_with("import") || (about_builds && tools.contains(&title.as_str()))
    });
    chosen.cloned().unwrap_or(Value::Null)
}

fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Where coursier puts launchers (`cs install metals`): `$COURSIER_BIN_DIR`, else its
/// Linux and macOS defaults. Often not on `PATH`.
fn coursier_bins() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("COURSIER_BIN_DIR").filter(|d| !d.is_empty()).map(PathBuf::from).into_iter().collect();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local/share/coursier/bin"));
        dirs.push(home.join("Library/Application Support/Coursier/bin"));
    }
    dirs
}

/// Where a server command's program is: `None` when it runs from `PATH` as written,
/// `Some(path)` for a coursier launcher outside `PATH`. `Err` when it is nowhere.
fn locate(cmd: &str) -> Result<Option<PathBuf>, ()> {
    let program = cmd.split_whitespace().next().ok_or(())?;
    if on_path(program) {
        return Ok(None);
    }
    if program.contains('/') {
        return Err(());
    }
    coursier_bins().into_iter().map(|d| d.join(program)).find(|p| p.is_file()).map(Some).ok_or(())
}

/// The rustup default toolchain name (`stable-aarch64-apple-darwin`), if rustup exists.
fn default_rustup_toolchain() -> Option<String> {
    let out = std::process::Command::new("rustup").arg("default").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().next().map(str::to_owned)
}

/// `file://` URI for an absolute path; everything but unreserved bytes and `/` is percent-encoded.
pub fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let bytes = uri.strip_prefix("file://")?.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%' && i + 3 <= bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match escaped {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok().map(PathBuf::from)
}

type Shared<T> = Arc<Mutex<T>>;
type Pending = Shared<HashMap<i64, oneshot::Sender<Result<Value, String>>>>;
type Stdin = Arc<tokio::sync::Mutex<ChildStdin>>;

struct Server {
    command: String,
    stdin: Stdin,
    pending: Pending,
    next_id: AtomicI64,
    opened: Mutex<HashSet<String>>,
    /// Active `$/progress` tokens (indexing, analysis).
    progress: Shared<HashSet<String>>,
    exited: Shared<Option<String>>,
    _child: Child,
}

async fn write_message(stdin: &Stdin, msg: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    let mut stdin = stdin.lock().await;
    stdin.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes()).await?;
    stdin.write_all(&body).await?;
    stdin.flush().await
}

impl Server {
    fn exit_reason(&self) -> Option<String> {
        self.exited.lock().unwrap().clone()
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        if let Some(reason) = self.exit_reason() {
            return Err(reason);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = write_message(&self.stdin, &msg).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(self.exit_reason().unwrap_or_else(|| format!("{}: {e}", self.command)));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self.exit_reason().unwrap_or_else(|| "language server stopped".into())),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("{method} timed out after {}s (the server may still be indexing)", timeout.as_secs()))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        write_message(&self.stdin, &msg).await.map_err(|e| format!("{}: {e}", self.command))
    }
}

/// How review-master answers the requests servers send it and wait on.
#[derive(Clone)]
struct Client {
    root_uri: String,
    import_builds: bool,
}

impl Client {
    fn answer(&self, method: &str, params: &Value) -> Value {
        match method {
            "workspace/configuration" => json!(vec![Value::Null; params["items"].as_array().map_or(0, Vec::len)]),
            "workspace/workspaceFolders" => json!([{ "uri": self.root_uri, "name": "workspace" }]),
            "window/showMessageRequest" => message_choice(params, self.import_builds),
            _ => Value::Null,
        }
    }
}

/// Completes pending requests, answers the server-to-client requests servers
/// wait on, and tracks progress. Ends when the server's stdout closes.
async fn read_loop(
    stdout: ChildStdout,
    stdin: Stdin,
    pending: Pending,
    progress: Shared<HashSet<String>>,
    exited: Shared<Option<String>>,
    stderr_tail: Shared<String>,
    client: Client,
) {
    let mut reader = BufReader::new(stdout);
    let mut header = String::new();
    'messages: loop {
        let mut length = None;
        loop {
            header.clear();
            match reader.read_line(&mut header).await {
                Ok(0) | Err(_) => break 'messages,
                Ok(_) => {}
            }
            let line = header.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                length = v.trim().parse::<usize>().ok();
            }
        }
        let Some(length) = length else { continue };
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).await.is_err() {
            break;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else { continue };
        match (msg["method"].as_str(), msg.get("id")) {
            (None, Some(id)) => {
                let tx = id.as_i64().and_then(|id| pending.lock().unwrap().remove(&id));
                if let Some(tx) = tx {
                    let result = match msg.get("error") {
                        Some(err) => Err(err["message"].as_str().unwrap_or("request failed").to_owned()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(result);
                }
            }
            (Some(method), Some(id)) => {
                let result = client.answer(method, &msg["params"]);
                let _ = write_message(&stdin, &json!({ "jsonrpc": "2.0", "id": id, "result": result })).await;
            }
            (Some("$/progress"), None) => {
                let token = msg["params"]["token"].to_string();
                match msg["params"]["value"]["kind"].as_str() {
                    Some("begin") => {
                        progress.lock().unwrap().insert(token);
                    }
                    Some("end") => {
                        progress.lock().unwrap().remove(&token);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    // Give the stderr reader a moment to capture why the server quit.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let tail = stderr_tail.lock().unwrap().clone();
    let reason = tail.lines().rev().find(|l| !l.trim().is_empty()).map(str::trim).map(str::to_owned);
    let reason = match reason {
        Some(line) => format!("language server exited: {line}"),
        None => "language server exited".to_owned(),
    };
    *exited.lock().unwrap() = Some(reason.clone());
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err(reason.clone()));
    }
}

#[derive(Serialize)]
pub struct OutlineItem {
    pub name: String,
    /// Signature or type, when the server gives one.
    pub detail: Option<String>,
    /// LSP symbol kind, lowercased (`function`, `class`, `struct`, …).
    pub kind: &'static str,
    /// 1-based line of the symbol's name.
    pub line: u64,
    pub depth: usize,
}

fn symbol_kind(kind: u64) -> &'static str {
    const KINDS: [&str; 26] = [
        "file", "module", "namespace", "package", "class", "method", "property", "field", "constructor", "enum",
        "interface", "function", "variable", "constant", "string", "number", "boolean", "array", "object", "key",
        "null", "enum member", "struct", "event", "operator", "type parameter",
    ];
    kind.checked_sub(1).and_then(|i| KINDS.get(i as usize)).copied().unwrap_or("symbol")
}

/// Declarations worth navigating to: types, functions and namespaces at any depth
/// (Rust `impl` blocks arrive as `object`), variables and constants only at the top
/// level. Locals, fields and members of dropped symbols are left out.
fn outline_items(symbols: &[Value], depth: usize, out: &mut Vec<OutlineItem>) {
    let line_of = |s: &Value| {
        let range = s.get("selectionRange").or_else(|| s.get("range")).or_else(|| s["location"].get("range"));
        range.and_then(|r| r["start"]["line"].as_u64()).unwrap_or(0) + 1
    };
    let mut sorted: Vec<&Value> = symbols.iter().collect();
    sorted.sort_by_key(|s| line_of(s));
    for s in sorted {
        let kind = symbol_kind(s["kind"].as_u64().unwrap_or(0));
        // Flat results only say which symbol contains them.
        let depth = if s.get("location").is_some() && s["containerName"].as_str().is_some_and(|c| !c.is_empty()) { 1 } else { depth };
        let structural = matches!(
            kind,
            "module" | "namespace" | "package" | "class" | "method" | "constructor" | "enum" | "interface" | "function" | "object" | "struct"
        );
        let top_level_value = depth == 0 && matches!(kind, "variable" | "constant");
        if !structural && !top_level_value {
            continue;
        }
        let Some(name) = s["name"].as_str() else { continue };
        out.push(OutlineItem {
            name: name.to_owned(),
            detail: s["detail"].as_str().filter(|d| !d.is_empty()).map(str::to_owned),
            kind,
            line: line_of(s),
            depth,
        });
        if let Some(children) = s["children"].as_array() {
            outline_items(children, depth + 1, out);
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Location {
    /// Repository-relative path, or `None` when the target is outside the repository.
    pub path: Option<String>,
    /// Path shown to the user (relative inside the repository, absolute outside).
    pub display: String,
    /// 1-based.
    pub line: u64,
    /// 0-based UTF-16 column, as LSP reports it.
    pub character: u64,
    pub preview: String,
}

#[derive(Serialize)]
pub struct SymbolInfo {
    pub hover: Option<String>,
    pub definitions: Vec<Location>,
}

#[derive(Serialize)]
pub struct Status {
    pub language: Option<&'static str>,
    pub server: Option<String>,
    /// `none` (no server for this file type), `missing` (none installed; `error` says how
    /// to get one), `idle` (not started), `starting`, `ready`, `indexing` or `failed`.
    pub state: &'static str,
    pub error: Option<String>,
}

/// One start attempt per language; concurrent first requests wait on the same cell
/// instead of holding the map lock, so status checks never block on a slow start.
type Slot = Arc<tokio::sync::OnceCell<Result<Arc<Server>, String>>>;

pub struct Lsp {
    root: PathBuf,
    client: Client,
    commands: HashMap<String, String>,
    /// Programs found outside `PATH` (coursier launchers), by language.
    programs: HashMap<String, PathBuf>,
    /// Languages with no server found (and none disabled on purpose).
    missing: Vec<&'static str>,
    slots: Mutex<HashMap<&'static str, Slot>>,
}

impl Lsp {
    /// `overrides` are `language=command` pairs; an empty command disables that language.
    /// `import_builds` lets servers import the project build (Metals), which runs the build tool.
    pub fn new(root: &Path, enabled: bool, overrides: &[(String, String)], import_builds: bool) -> Lsp {
        let mut commands = HashMap::new();
        let mut programs = HashMap::new();
        let mut missing = Vec::new();
        if enabled {
            for (lang, candidates) in DEFAULT_SERVERS {
                match candidates.iter().find_map(|c| locate(c).ok().map(|at| (c, at))) {
                    Some((cmd, at)) => {
                        commands.insert((*lang).to_owned(), (*cmd).to_owned());
                        if let Some(path) = at {
                            programs.insert((*lang).to_owned(), path);
                        }
                    }
                    None => missing.push(*lang),
                }
            }
            for (lang, cmd) in overrides {
                programs.remove(lang);
                missing.retain(|l| l != lang);
                if cmd.trim().is_empty() {
                    commands.remove(lang);
                } else {
                    commands.insert(lang.clone(), cmd.trim().to_owned());
                }
            }
        }
        let client = Client { root_uri: path_to_uri(root), import_builds };
        Lsp { root: root.to_owned(), client, commands, programs, missing, slots: Default::default() }
    }

    /// Configured (language, command) pairs, for the startup banner.
    pub fn configured(&self) -> Vec<(String, String)> {
        let mut list: Vec<_> = self
            .commands
            .iter()
            .map(|(l, c)| match self.programs.get(l) {
                Some(path) => (l.clone(), path.display().to_string()),
                None => (l.clone(), c.clone()),
            })
            .collect();
        list.sort();
        list
    }

    /// Languages among `paths` that have no server installed, with how to get one.
    pub fn missing_for<'a>(&self, paths: impl Iterator<Item = &'a str>) -> Vec<(&'static str, &'static str)> {
        let mut langs: Vec<&'static str> = paths.filter_map(|p| language_of(p).map(|(l, _)| l)).filter(|l| self.missing.contains(l)).collect();
        langs.sort_unstable();
        langs.dedup();
        langs.into_iter().map(|l| (l, install_hint(l))).collect()
    }

    pub async fn status(&self, path: &str) -> Status {
        let Some((lang, _)) = language_of(path) else {
            return Status { language: None, server: None, state: "none", error: None };
        };
        let Some(cmd) = self.commands.get(lang) else {
            if self.missing.contains(&lang) {
                let error = format!("no {lang} language server found; install one: {}", install_hint(lang));
                return Status { language: Some(lang), server: None, state: "missing", error: Some(error) };
            }
            return Status { language: Some(lang), server: None, state: "none", error: None };
        };
        let slot = self.slots.lock().unwrap().get(lang).cloned();
        let (state, error) = match slot.as_ref().map(|s| s.get()) {
            None => ("idle", None),
            Some(None) => ("starting", None),
            Some(Some(Err(e))) => ("failed", Some(e.clone())),
            Some(Some(Ok(s))) => match s.exit_reason() {
                Some(e) => ("failed", Some(e)),
                None if !s.progress.lock().unwrap().is_empty() => ("indexing", None),
                None => ("ready", None),
            },
        };
        Status { language: Some(lang), server: Some(cmd.clone()), state, error }
    }

    async fn server(&self, lang: &'static str) -> Result<Arc<Server>, String> {
        let cmd = self.commands.get(lang).ok_or_else(|| format!("no language server configured for {lang}"))?;
        let slot = self.slots.lock().unwrap().entry(lang).or_default().clone();
        let server = slot.get_or_init(|| self.start(lang, cmd)).await.clone()?;
        server.exit_reason().map_or(Ok(server), Err)
    }

    async fn start(&self, lang: &str, cmd: &str) -> Result<Arc<Server>, String> {
        match self.start_with(lang, cmd, None).await {
            // The rustup shim refuses to run when the project pins a toolchain
            // without the rust-analyzer component (`profile = "minimal"`); retry on
            // the default toolchain — navigation-compatible — and say how to fix it.
            Err(e) if e.contains("Unknown binary 'rust-analyzer'") => {
                let hint = format!("{e}; fix: `rustup component add rust-analyzer --toolchain <the pinned toolchain>`");
                let Some(default) = default_rustup_toolchain() else { return Err(hint) };
                self.start_with(lang, cmd, Some(("RUSTUP_TOOLCHAIN", default))).await.map_err(|_| hint)
            }
            other => other,
        }
    }

    async fn start_with(&self, lang: &str, cmd: &str, env: Option<(&'static str, String)>) -> Result<Arc<Server>, String> {
        let argv: Vec<&str> = cmd.split_whitespace().collect();
        let mut command = match self.programs.get(lang) {
            Some(path) => Command::new(path),
            None => Command::new(argv[0]),
        };
        command
            .args(&argv[1..])
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some((key, value)) = &env {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|e| format!("cannot start `{cmd}`: {e}"))?;
        let stdin: Stdin = Arc::new(tokio::sync::Mutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");

        let stderr_tail: Shared<String> = Arc::default();
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut chunk).await {
                let mut t = tail.lock().unwrap();
                t.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if t.len() > 8192 {
                    let mut cut = t.len() - 8192;
                    while !t.is_char_boundary(cut) {
                        cut += 1;
                    }
                    t.drain(..cut);
                }
            }
        });

        let server = Arc::new(Server {
            command: cmd.to_owned(),
            stdin: stdin.clone(),
            pending: Arc::default(),
            next_id: AtomicI64::new(1),
            opened: Mutex::default(),
            progress: Arc::default(),
            exited: Arc::default(),
            _child: child,
        });
        tokio::spawn(read_loop(
            stdout,
            stdin,
            server.pending.clone(),
            server.progress.clone(),
            server.exited.clone(),
            stderr_tail,
            self.client.clone(),
        ));

        let folder = json!([{ "uri": self.client.root_uri, "name": "workspace" }]);
        let params = json!({
            "processId": std::process::id(),
            "rootUri": self.client.root_uri,
            "rootPath": self.root,
            "workspaceFolders": folder,
            "initializationOptions": initialization_options(lang),
            "capabilities": {
                "general": { "positionEncodings": ["utf-16"] },
                "textDocument": {
                    "synchronization": { "didSave": false },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "linkSupport": true },
                    "references": {},
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true }
                },
                "window": { "workDoneProgress": true },
                "workspace": { "configuration": true, "workspaceFolders": true }
            }
        });
        let init = server
            .request("initialize", params, INIT_TIMEOUT)
            .await
            .map_err(|e| format!("`{cmd}` failed to initialize: {e}"))?;
        server.notify("initialized", json!({})).await?;
        self.import_build(&server, &init);
        Ok(server)
    }

    /// With `--lsp-import-build`, ask a server that offers `build-import` (Metals) to
    /// import the project build, unless one was imported before (`.bloop/`): Metals
    /// reconnects to that by itself. The prompt is not relied on, since Metals stops
    /// asking once an earlier read-only run dismissed it.
    fn import_build(&self, server: &Arc<Server>, init: &Value) {
        let commands = init["capabilities"]["executeCommandProvider"]["commands"].as_array();
        let offered = commands.is_some_and(|c| c.iter().any(|c| c == "build-import"));
        if !self.client.import_builds || !offered || self.root.join(".bloop").exists() {
            return;
        }
        let server = server.clone();
        tokio::spawn(async move {
            let params = json!({ "command": "build-import", "arguments": [] });
            let _ = server.request("workspace/executeCommand", params, BUILD_IMPORT_TIMEOUT).await;
        });
    }

    async fn open(&self, full: &Path, rel: &str) -> Result<(Arc<Server>, String), String> {
        let (lang, language_id) = language_of(rel).ok_or("no language server for this file type")?;
        let server = self.server(lang).await?;
        let uri = path_to_uri(full);
        let first = server.opened.lock().unwrap().insert(uri.clone());
        if first {
            let text = fs::read_to_string(full).map_err(|e| format!("{rel}: {e}"))?;
            let doc = json!({ "uri": uri, "languageId": language_id, "version": 1, "text": text });
            server.notify("textDocument/didOpen", json!({ "textDocument": doc })).await?;
        }
        Ok((server, uri))
    }

    fn position(uri: &str, line: u64, character: u64) -> Value {
        json!({ "textDocument": { "uri": uri }, "position": { "line": line.saturating_sub(1), "character": character } })
    }

    /// Start the file's server and open the document so analysis begins before the first click.
    pub async fn warm(&self, full: &Path, rel: &str) {
        let _ = self.open(full, rel).await;
    }

    /// Hover text and definitions for the symbol at a 1-based line and UTF-16 column.
    pub async fn symbol(&self, full: &Path, rel: &str, line: u64, character: u64) -> Result<SymbolInfo, String> {
        let (server, uri) = self.open(full, rel).await?;
        let at = Self::position(&uri, line, character);
        let (hover, definition) = tokio::join!(
            server.request("textDocument/hover", at.clone(), REQUEST_TIMEOUT),
            server.request("textDocument/definition", at, REQUEST_TIMEOUT),
        );
        let definitions = self.locations(&definition?);
        Ok(SymbolInfo { hover: hover.ok().as_ref().and_then(hover_text), definitions })
    }

    pub async fn references(&self, full: &Path, rel: &str, line: u64, character: u64) -> Result<Vec<Location>, String> {
        let (server, uri) = self.open(full, rel).await?;
        let mut params = Self::position(&uri, line, character);
        params["context"] = json!({ "includeDeclaration": true });
        let result = server.request("textDocument/references", params, REQUEST_TIMEOUT).await?;
        let mut refs = self.locations(&result);
        refs.truncate(MAX_REFERENCES);
        Ok(refs)
    }

    /// Raw `textDocument/documentSymbol` result (hierarchical).
    pub async fn document_symbols(&self, full: &Path, rel: &str) -> Result<Vec<Value>, String> {
        let (server, uri) = self.open(full, rel).await?;
        let params = json!({ "textDocument": { "uri": uri } });
        let result = server.request("textDocument/documentSymbol", params, REQUEST_TIMEOUT).await?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    /// The file's declarations, in source order, nested by depth.
    pub async fn outline(&self, full: &Path, rel: &str) -> Result<Vec<OutlineItem>, String> {
        let symbols = self.document_symbols(full, rel).await?;
        let mut out = Vec::new();
        outline_items(&symbols, 0, &mut out);
        Ok(out)
    }

    /// Accepts `Location`, `Location[]` and `LocationLink[]` results.
    fn locations(&self, result: &Value) -> Vec<Location> {
        let items: Vec<&Value> = match result {
            Value::Array(items) => items.iter().collect(),
            Value::Object(_) => vec![result],
            _ => vec![],
        };
        let mut lines_cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut out = Vec::new();
        for item in items {
            let (uri, range) = match item.get("targetUri") {
                Some(uri) => (uri, item.get("targetSelectionRange").or_else(|| item.get("targetRange"))),
                None => (&item["uri"], item.get("range")),
            };
            let Some(path) = uri.as_str().and_then(uri_to_path) else { continue };
            let start = &range.cloned().unwrap_or_default()["start"];
            let line = start["line"].as_u64().unwrap_or(0) + 1;
            let character = start["character"].as_u64().unwrap_or(0);
            let inside = path.canonicalize().ok().filter(|p| p.starts_with(&self.root));
            let rel = inside
                .as_ref()
                .and_then(|p| p.strip_prefix(&self.root).ok())
                .map(|p| p.to_string_lossy().into_owned());
            let preview = inside
                .as_ref()
                .and_then(|p| {
                    let lines = lines_cache.entry(p.clone()).or_insert_with(|| {
                        fs::read_to_string(p).map(|t| t.lines().map(str::to_owned).collect()).unwrap_or_default()
                    });
                    lines.get(line as usize - 1).map(|l| l.trim().chars().take(200).collect())
                })
                .unwrap_or_default();
            out.push(Location {
                display: rel.clone().unwrap_or_else(|| path.display().to_string()),
                path: rel,
                line,
                character,
                preview,
            });
        }
        out
    }
}

fn hover_text(result: &Value) -> Option<String> {
    fn part(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => {
                let value = o.get("value")?.as_str()?;
                match o.get("language").and_then(Value::as_str) {
                    Some(lang) => Some(format!("```{lang}\n{value}\n```")),
                    None => Some(value.to_owned()),
                }
            }
            _ => None,
        }
    }
    let contents = result.get("contents")?;
    let text = match contents {
        Value::Array(parts) => parts.iter().filter_map(part).collect::<Vec<_>>().join("\n\n"),
        other => part(other)?,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip_with_reserved_characters() {
        let path = Path::new("/tmp/a dir/ü#?.py");
        let uri = path_to_uri(path);
        assert_eq!(uri, "file:///tmp/a%20dir/%C3%BC%23%3F.py");
        assert_eq!(uri_to_path(&uri).unwrap(), path);
        assert_eq!(uri_to_path("file:///x/100%").unwrap(), Path::new("/x/100%"));
    }

    #[test]
    fn build_imports_need_the_opt_in() {
        let import = json!({ "message": "New sbt workspace detected, would you like to import the build?",
            "actions": [{ "title": "Import build" }, { "title": "Not now" }, { "title": "Don't show again" }] });
        assert_eq!(message_choice(&import, false), Value::Null);
        assert_eq!(message_choice(&import, true), json!({ "title": "Import build" }));
        let tool = json!({ "message": "Multiple build definitions found. Which would you like to use?",
            "actions": [{ "title": "mill" }, { "title": "sbt" }] });
        assert_eq!(message_choice(&tool, true), json!({ "title": "mill" }));
        let other = json!({ "message": "Upgrade to Scala 3?", "actions": [{ "title": "Yes" }] });
        assert_eq!(message_choice(&other, true), Value::Null);
    }
}
