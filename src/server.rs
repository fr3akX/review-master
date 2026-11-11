//! Local HTTP API and embedded web UI: browse the change, chat with the engine
//! about exact lines, keep draft comments, submit one GitHub review. Engine
//! turns run as background jobs the UI polls.

use crate::diff::{Diff, Kind, Model, Side, Status};
use crate::engine::Engine;
use crate::lsp::Lsp;
use crate::pr::{PrMeta, PrRef};
use crate::review::{self, Finding};
use crate::{findings, flows, post, search, verify};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post as post_to};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::Notify;

const MAX_PROGRESS: usize = 5000;
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;
/// Head-file context shown to the engine around a chat anchor.
const CHAT_CONTEXT_LINES: u64 = 20;

pub struct AppState {
    /// Checkout the review runs over (the PR-head worktree, or the repo itself).
    pub root: PathBuf,
    /// `Some` in PR mode; `None` when reviewing a bare worktree.
    pub pr: Option<(PrRef, PrMeta)>,
    pub head_sha: String,
    pub branch: String,
    pub dirty: bool,
    /// The whole change (after command-line excludes); the scope filters it.
    full: Model,
    /// Files dropped on the command line, for the chat prompt note.
    cli_skipped: Vec<String>,
    /// What this review currently covers (runtime exclusions applied).
    scope: Mutex<Scope>,
    scope_file: PathBuf,
    summary: Mutex<SummaryState>,
    summary_file: PathBuf,
    flows: Mutex<FlowsState>,
    flows_file: PathBuf,
    findings: Mutex<findings::Store>,
    pub engine: Engine,
    /// The `gh` login, for own-PR hints in the UI.
    pub viewer: Option<String>,
    pub lsp: Lsp,
    drafts: Mutex<DraftStore>,
    threads: Mutex<Threads>,
    jobs: Mutex<Jobs>,
    submitted: Mutex<Option<String>>,
    /// Review comments already on the PR, fetched on first use.
    comments: Mutex<Option<Vec<Value>>>,
    /// Stops the server: the UI's Exit (Ctrl-C does the same in the terminal).
    shutdown: Notify,
}

/// The files this review currently covers.
struct Scope {
    model: Model,
    anchors: Diff,
    /// Runtime-excluded exact paths, persisted across restarts.
    excluded: Vec<String>,
}

fn build_scope(full: &Model, excluded: Vec<String>) -> Scope {
    let model = Model { files: full.files.iter().filter(|f| !excluded.contains(&f.path)).cloned().collect() };
    let anchors = model.anchors();
    Scope { model, anchors, excluded }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct StoredScope {
    excluded: Vec<String>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        root: PathBuf,
        pr: Option<(PrRef, PrMeta)>,
        head_sha: String,
        branch: String,
        dirty: bool,
        model: Model,
        skipped: Vec<String>,
        engine: Engine,
        viewer: Option<String>,
        drafts_file: PathBuf,
        scope_file: PathBuf,
        summary_file: PathBuf,
        threads_file: PathBuf,
        findings_file: PathBuf,
        flows_file: PathBuf,
        lsp: Lsp,
    ) -> Arc<AppState> {
        let stored: StoredScope = fs::read(&scope_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let excluded: Vec<String> = stored.excluded.into_iter().filter(|p| model.file(p).is_some()).collect();
        let scope = build_scope(&model, excluded);
        let summary_text = fs::read(&summary_file)
            .ok()
            .and_then(|b| serde_json::from_slice::<StoredSummary>(&b).ok())
            .filter(|s| s.head == head_sha)
            .map(|s| s.summary);
        let stored_flows = fs::read(&flows_file)
            .ok()
            .and_then(|b| serde_json::from_slice::<StoredFlows>(&b).ok())
            .filter(|s| s.head == head_sha && s.version == flows::VERSION)
            .map(|s| s.flows);
        Arc::new(AppState {
            root,
            pr,
            head_sha,
            branch,
            dirty,
            full: model,
            cli_skipped: skipped,
            scope: Mutex::new(scope),
            scope_file,
            summary: Mutex::new(SummaryState { text: summary_text, job: None }),
            summary_file,
            flows: Mutex::new(FlowsState { flows: stored_flows, job: None }),
            flows_file,
            findings: Mutex::new(findings::Store::load(findings_file)),
            engine,
            viewer,
            lsp,
            drafts: Mutex::new(DraftStore::load(drafts_file)),
            threads: Mutex::new(Threads::load(threads_file)),
            jobs: Mutex::default(),
            submitted: Mutex::new(None),
            comments: Mutex::new(None),
            shutdown: Notify::new(),
        })
    }

    fn subject(&self) -> review::Subject {
        match &self.pr {
            Some((r, m)) => review::Subject::pr(r, m, &self.head_sha),
            None => review::Subject::local(&self.branch, &self.head_sha, self.dirty),
        }
    }

    /// Import an LLM review (Markdown) as findings, replacing an earlier import of the same name.
    pub fn import_findings(&self, name: &str, text: &str) -> findings::Imported {
        let mut store = self.findings.lock().unwrap();
        let imported = store.import(name, text, &self.root, &self.full);
        store.save();
        imported
    }

    /// Everything not in this review, for the chat prompt.
    fn out_of_scope(&self, scope: &Scope) -> Vec<String> {
        self.cli_skipped.iter().chain(scope.excluded.iter()).cloned().collect()
    }

    fn save_scope(&self, excluded: &[String]) {
        let stored = StoredScope { excluded: excluded.to_vec() };
        if let Ok(bytes) = serde_json::to_vec_pretty(&stored) {
            let _ = fs::write(&self.scope_file, bytes);
        }
    }
}

// ---- drafts -----------------------------------------------------------------

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Draft {
    id: u64,
    path: String,
    line: u64,
    #[serde(default)]
    start: Option<u64>,
    side: String,
    severity: String,
    body: String,
    #[serde(default)]
    problem: Option<String>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct DraftStore {
    next: u64,
    items: Vec<Draft>,
    #[serde(skip)]
    file: PathBuf,
}

impl DraftStore {
    /// Drafts survive restarts: they live under the cache, keyed by PR/branch.
    fn load(file: PathBuf) -> DraftStore {
        let mut store: DraftStore =
            fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        store.file = file;
        store
    }

    fn save(&self) {
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = fs::write(&self.file, bytes);
        }
    }
}

// ---- chat threads -----------------------------------------------------------

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Anchor {
    path: String,
    start: Option<u64>,
    line: u64,
    side: Side,
}

impl Anchor {
    fn json(&self) -> Value {
        json!({ "path": self.path, "start": self.start, "line": self.line, "side": self.side.api() })
    }

    fn location(&self) -> String {
        match self.start {
            Some(s) => format!("{}:{}-{}", self.path, s, self.line),
            None => format!("{}:{}", self.path, self.line),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Thread {
    id: u64,
    anchor: Option<Anchor>,
    session: String,
    /// (role, text)
    messages: Vec<(String, String)>,
    #[serde(skip)]
    busy: bool,
    /// The running turn's job, so any page can stop it.
    #[serde(skip)]
    job: Option<u64>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Threads {
    next: u64,
    by_id: HashMap<u64, Thread>,
    #[serde(skip)]
    file: PathBuf,
}

impl Threads {
    /// Conversations survive restarts like drafts do; their engine sessions
    /// are persisted by the engines themselves.
    fn load(file: PathBuf) -> Threads {
        let mut threads: Threads = fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        threads.file = file;
        threads
    }

    fn save(&self) {
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = fs::write(&self.file, bytes);
        }
    }
}

// ---- jobs -------------------------------------------------------------------

#[derive(Default)]
struct Jobs {
    next_id: u64,
    by_id: HashMap<u64, Arc<Job>>,
}

struct Job {
    id: u64,
    cancel: Notify,
    inner: Mutex<JobInner>,
}

#[derive(Default)]
struct JobInner {
    done: bool,
    progress: Vec<String>,
    error: Option<String>,
    result: Option<Value>,
}

impl Job {
    fn push(&self, line: String) {
        let mut inner = self.inner.lock().unwrap();
        if inner.progress.len() < MAX_PROGRESS {
            inner.progress.push(line);
        }
    }

    fn finish(&self, result: Result<Value, String>) {
        let mut inner = self.inner.lock().unwrap();
        inner.done = true;
        match result {
            Ok(value) => inner.result = Some(value),
            Err(e) => inner.error = Some(e),
        }
    }
}

fn new_job(jobs: &mut Jobs) -> Arc<Job> {
    jobs.next_id += 1;
    let job = Arc::new(Job { id: jobs.next_id, cancel: Notify::new(), inner: Mutex::default() });
    jobs.by_id.insert(job.id, job.clone());
    job
}

// ---- routing ----------------------------------------------------------------

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn fail(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

pub async fn serve(listener: TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    let app = Router::new()
        .route("/", get(|| asset("text/html; charset=utf-8", include_str!("../web/index.html"))))
        .route("/app.js", get(|| asset("text/javascript; charset=utf-8", include_str!("../web/app.js"))))
        .route("/style.css", get(|| asset("text/css; charset=utf-8", include_str!("../web/style.css"))))
        .route("/vendor/highlight.js", get(|| asset("text/javascript; charset=utf-8", include_str!("../web/vendor/highlight.js"))))
        .route("/vendor/highlight.css", get(|| asset("text/css; charset=utf-8", include_str!("../web/vendor/highlight.css"))))
        .route("/api/info", get(info))
        .route("/api/diff", get(file_diff))
        .route("/api/source", get(source))
        .route("/api/chat", post_to(chat))
        .route("/api/thread", get(thread))
        .route("/api/thread/{id}", delete(delete_thread))
        .route("/api/threads", get(threads))
        .route("/api/job/{id}", get(job_status))
        .route("/api/job/{id}/cancel", post_to(cancel_job))
        .route("/api/comments", get(pr_comments))
        .route("/api/comments/reply", post_to(reply_comment))
        .route("/api/draft", post_to(upsert_draft))
        .route("/api/draft/{id}", delete(delete_draft))
        .route("/api/drafts", get(drafts))
        .route("/api/submit", post_to(submit))
        .route("/api/scope", post_to(set_scope))
        .route("/api/summary", get(get_summary).post(regenerate_summary))
        .route("/api/flows", get(get_flows).post(generate_flows))
        .route("/api/findings", get(list_findings).post(import_findings).delete(remove_findings))
        .route("/api/findings/{id}", post_to(set_finding_status))
        .route("/api/folds", get(folds))
        .route("/api/search", get(text_search))
        .route("/api/lsp/status", get(lsp_status))
        .route("/api/lsp/symbol", post_to(lsp_symbol))
        .route("/api/lsp/references", post_to(lsp_references))
        .route("/api/lsp/outline", get(lsp_outline))
        .route("/api/shutdown", post_to(shutdown))
        .with_state(state.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = state.shutdown.notified() => println!("review-master: stopped from the browser"),
            }
        })
        .await
}

/// Stop serving, once this response is on its way (the UI's Exit button).
async fn shutdown(State(st): State<Arc<AppState>>) -> Json<Value> {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        st.shutdown.notify_one();
    });
    Json(json!({}))
}

async fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

// ---- handlers ---------------------------------------------------------------

fn files_json(model: &Model) -> Vec<Value> {
    model
        .files
        .iter()
        .map(|f| {
            // Identifies this file's diff, so "viewed" marks reset when it changes.
            let mut text = String::new();
            Model::render_file(f, &mut text);
            json!({
                "path": f.path,
                "status": f.status.api(),
                "old_path": f.old_path,
                "additions": f.additions,
                "deletions": f.deletions,
                "sig": format!("{:016x}", xxhash_rust::xxh64::xxh64(text.as_bytes(), 0)),
            })
        })
        .collect()
}

async fn info(State(st): State<Arc<AppState>>) -> Json<Value> {
    let (files, excluded) = {
        let scope = st.scope.lock().unwrap();
        (files_json(&scope.model), scope.excluded.clone())
    };
    let pr = st.pr.as_ref().map(|(r, m)| {
        json!({
            "owner": r.owner, "repo": r.repo, "number": r.number,
            "title": m.title, "author": m.author, "url": m.url,
            "state": m.state, "draft": m.draft, "merged": m.merged,
            "base_ref": m.base_ref, "head_ref": m.head_ref,
        })
    });
    Json(json!({
        "mode": if st.pr.is_some() { "pr" } else { "local" },
        "pr": pr,
        "head": st.head_sha,
        "branch": st.branch,
        "dirty": st.dirty,
        "engine": { "kind": st.engine.kind().id(), "model": st.engine.model() },
        "viewer": st.viewer,
        "files": files,
        "skipped": st.cli_skipped,
        "excluded": excluded,
        "submitted": *st.submitted.lock().unwrap(),
    }))
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
}

async fn file_diff(State(st): State<Arc<AppState>>, Query(q): Query<PathQuery>) -> ApiResult {
    let scope = st.scope.lock().unwrap();
    let file = scope.model.file(&q.path).ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("`{}` is not part of the review", q.path)))?;
    let hunks: Vec<Value> = file
        .hunks
        .iter()
        .map(|h| {
            let lines: Vec<Value> = h
                .lines
                .iter()
                .map(|l| {
                    let k = match l.kind {
                        Kind::Ctx => "ctx",
                        Kind::Add => "add",
                        Kind::Del => "del",
                    };
                    json!({ "k": k, "old": l.old, "new": l.new, "text": l.text })
                })
                .collect();
            json!({ "header": h.header, "lines": lines })
        })
        .collect();
    // Head-file line count, so the UI can offer the unchanged lines below the last hunk.
    let new_lines = (file.status != Status::Deleted)
        .then(|| verify::resolve(&st.root, &q.path).ok())
        .flatten()
        .filter(|(full, _)| fs::metadata(full).is_ok_and(|m| m.is_file() && m.len() <= MAX_SOURCE_BYTES))
        .and_then(|(full, _)| fs::read(full).ok())
        .map(|bytes| verify::line_count(&bytes));
    Ok(Json(json!({ "path": file.path, "hunks": hunks, "new_lines": new_lines })))
}

/// Language hint for the UI: the lowercased file extension.
fn lang(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path).rsplit_once('.').map_or("", |(_, ext)| ext)
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    /// Whole identifiers only (case-sensitive) instead of smart-case substrings.
    #[serde(default)]
    word: bool,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    300
}

async fn text_search(State(st): State<Arc<AppState>>, Query(q): Query<SearchQuery>) -> ApiResult {
    let text = if q.word { q.q.trim() } else { q.q.as_str() }.to_owned();
    if text.trim().is_empty() || text.len() > 200 {
        return Err(fail(StatusCode::BAD_REQUEST, "search for 1 to 200 characters"));
    }
    if q.word && text.contains(char::is_whitespace) {
        return Err(fail(StatusCode::BAD_REQUEST, "search for a single word"));
    }
    let query = if q.word { search::Query::Word(text) } else { search::Query::Text(text) };
    let root = st.root.clone();
    let limit = q.limit.clamp(1, 1000);
    let results = tokio::task::spawn_blocking(move || search::search(&root, &query, limit))
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!(results)))
}

#[derive(Deserialize)]
struct StatusQuery {
    path: String,
    /// Start the server (and open the file) in the background if it is not running.
    #[serde(default)]
    start: bool,
}

async fn lsp_status(State(st): State<Arc<AppState>>, Query(q): Query<StatusQuery>) -> ApiResult {
    let (full, shown) = verify::resolve(&st.root, &q.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let status = st.lsp.status(&shown).await;
    if q.start && status.state == "idle" {
        let st = st.clone();
        tokio::spawn(async move { st.lsp.warm(&full, &shown).await });
    }
    Ok(Json(json!(status)))
}

#[derive(Deserialize)]
struct PositionRequest {
    path: String,
    /// 1-based line.
    line: u64,
    /// 0-based UTF-16 column.
    character: u64,
}

async fn lsp_symbol(State(st): State<Arc<AppState>>, Json(req): Json<PositionRequest>) -> ApiResult {
    let (full, shown) = verify::resolve(&st.root, &req.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let info = st.lsp.symbol(&full, &shown, req.line, req.character).await;
    info.map(|i| Json(json!(i))).map_err(|e| fail(StatusCode::BAD_GATEWAY, e))
}

async fn lsp_references(State(st): State<Arc<AppState>>, Json(req): Json<PositionRequest>) -> ApiResult {
    let (full, shown) = verify::resolve(&st.root, &req.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let refs = st.lsp.references(&full, &shown, req.line, req.character).await;
    refs.map(|r| Json(json!({ "references": r }))).map_err(|e| fail(StatusCode::BAD_GATEWAY, e))
}

/// Functions, classes and other declarations in a file, from its language server.
async fn lsp_outline(State(st): State<Arc<AppState>>, Query(q): Query<PathQuery>) -> ApiResult {
    let (full, shown) = verify::resolve(&st.root, &q.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let items = st.lsp.outline(&full, &shown).await;
    items.map(|i| Json(json!({ "symbols": i }))).map_err(|e| fail(StatusCode::BAD_GATEWAY, e))
}

async fn source(State(st): State<Arc<AppState>>, Query(q): Query<PathQuery>) -> ApiResult {
    let scope = st.scope.lock().unwrap();
    if scope.model.file(&q.path).is_some_and(|f| f.status == Status::Deleted) {
        return Err(fail(StatusCode::GONE, format!("`{}` is deleted by this change; use the diff view", q.path)));
    }
    let (full, shown) = verify::resolve(&st.root, &q.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let meta = fs::metadata(&full).map_err(|e| fail(StatusCode::NOT_FOUND, e.to_string()))?;
    if !meta.is_file() {
        return Err(fail(StatusCode::BAD_REQUEST, format!("{shown} is not a file")));
    }
    if meta.len() > MAX_SOURCE_BYTES {
        return Err(fail(StatusCode::PAYLOAD_TOO_LARGE, format!("{shown} is larger than 2 MiB")));
    }
    let bytes = fs::read(&full).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err(fail(StatusCode::UNSUPPORTED_MEDIA_TYPE, format!("{shown} is a binary file")));
    }
    let mut added: Vec<u64> = Vec::new();
    let mut deleted_after: HashMap<String, u64> = HashMap::new();
    if let Some(file) = scope.model.file(&q.path) {
        for hunk in &file.hunks {
            // Anchor deletions to the preceding surviving (new-numbered) line.
            let mut last_new = hunk.lines.iter().find_map(|l| l.new).unwrap_or(1).saturating_sub(1);
            for line in &hunk.lines {
                match line.kind {
                    Kind::Add => added.push(line.new.expect("added lines have new numbers")),
                    Kind::Del => *deleted_after.entry(last_new.to_string()).or_default() += 1,
                    Kind::Ctx => {}
                }
                if let Some(new) = line.new {
                    last_new = new;
                }
            }
        }
    }
    Ok(Json(json!({
        "path": shown,
        "text": String::from_utf8_lossy(&bytes),
        "added": added,
        "deleted_after": deleted_after,
        "lang": lang(&q.path),
    })))
}

#[derive(Deserialize)]
struct ChatRequest {
    #[serde(default)]
    thread: Option<u64>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    start: Option<u64>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    side: Option<String>,
    /// Exact code text the reviewer highlighted, for the prompt.
    #[serde(default)]
    snippet: Option<String>,
    question: String,
}

fn chat_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["answer"],
        "properties": { "answer": { "type": "string", "minLength": 1, "description": "the reply, concise Markdown" } }
    })
}

#[derive(Deserialize)]
struct ChatAnswer {
    answer: String,
}

/// The rendered diff hunk(s) containing the anchor.
fn anchored_hunks(model: &Model, a: &Anchor) -> String {
    let Some(file) = model.file(&a.path) else { return String::new() };
    let mut out = String::new();
    for hunk in &file.hunks {
        let hit = hunk.lines.iter().any(|l| match a.side {
            Side::Right => l.new.is_some_and(|n| n == a.line || a.start.is_some_and(|s| (s..=a.line).contains(&n))),
            Side::Left => l.old.is_some_and(|n| n == a.line || a.start.is_some_and(|s| (s..=a.line).contains(&n))),
        });
        if !hit {
            continue;
        }
        out.push_str(&hunk.header);
        out.push('\n');
        for line in &hunk.lines {
            let prefix = match line.kind {
                Kind::Ctx => ' ',
                Kind::Add => '+',
                Kind::Del => '-',
            };
            out.push(prefix);
            out.push_str(&line.text);
            out.push('\n');
        }
    }
    out
}

/// Numbered head-file window around the anchor (RIGHT side only).
fn file_window(st: &AppState, a: &Anchor) -> String {
    if a.side == Side::Left {
        return String::new();
    }
    let Ok((full, _)) = verify::resolve(&st.root, &a.path) else { return String::new() };
    let Ok(bytes) = fs::read(&full) else { return String::new() };
    let text = String::from_utf8_lossy(&bytes);
    let first = a.start.unwrap_or(a.line).saturating_sub(CHAT_CONTEXT_LINES).max(1);
    let last = a.line + CHAT_CONTEXT_LINES;
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        let n = i as u64 + 1;
        if n < first {
            continue;
        }
        if n > last {
            break;
        }
        out.push_str(&format!("{n}\t{line}\n"));
    }
    out
}

/// Chat context: subject, PR description, then anchor hunks/excerpt or the file list.
fn chat_prompt(st: &AppState, anchor: Option<&Anchor>, snippet: Option<&str>, question: &str) -> String {
    let subject = st.subject();
    let mut p = subject.heading;
    let description: String = subject.description.trim().chars().take(4000).collect();
    if !description.is_empty() {
        p.push_str(&format!("\n<description>\n{description}\n</description>\n"));
    }
    let scope = st.scope.lock().unwrap();
    match anchor {
        Some(a) => {
            p.push_str(&format!("\nThe reviewer selected {} ({} side of the diff).\n", a.location(), a.side.api()));
            let hunks = anchored_hunks(&scope.model, a);
            if !hunks.is_empty() {
                p.push_str(&format!("\n<diff-hunk>\n{hunks}</diff-hunk>\n"));
            }
            let window = file_window(st, a);
            if !window.is_empty() {
                p.push_str(&format!("\n<head-file-excerpt>\n{window}</head-file-excerpt>\n"));
            }
        }
        None => {
            let list: Vec<String> = scope.model.files.iter().map(|f| format!("{} ({})", f.path, f.status.api())).collect();
            p.push_str(&format!("\nChanged files: {}\n", list.join(", ")));
        }
    }
    let out = st.out_of_scope(&scope);
    drop(scope);
    if !out.is_empty() {
        p.push_str(&format!("\nExcluded from this review (the reviewer chose not to review them; ignore them): {}\n", out.join(", ")));
    }
    if let Some(snippet) = snippet.map(str::trim).filter(|s| !s.is_empty()) {
        let cut: String = snippet.chars().take(2000).collect();
        p.push_str(&format!("\nThe reviewer highlighted exactly this part:\n<selection>\n{cut}\n</selection>\n"));
    }
    p.push_str(
        "\nAnswer the reviewer's question about this change. Investigate with the repository tools when the \
shown context is not enough; cite `path:line` for claims about code. Be concrete and concise; Markdown, \
code spans for identifiers, fenced blocks only for real code.\n",
    );
    p.push_str(&format!("\nQuestion: {question}\n"));
    p
}

async fn chat(State(st): State<Arc<AppState>>, Json(req): Json<ChatRequest>) -> ApiResult {
    if req.question.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "the question is empty"));
    }
    // Resolve or create the thread (session creation must happen outside the lock).
    let (thread_id, session, prompt) = match req.thread {
        Some(id) => {
            let mut threads = st.threads.lock().unwrap();
            let t = threads.by_id.get_mut(&id).ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no thread {id}")))?;
            if t.busy {
                return Err(fail(StatusCode::CONFLICT, "this thread is already answering"));
            }
            t.busy = true;
            t.messages.push(("user".to_owned(), req.question.clone()));
            (id, t.session.clone(), format!("Follow-up question from the reviewer: {}\n", req.question))
        }
        None => {
            let anchor = match &req.path {
                Some(path) => {
                    let line = req.line.ok_or_else(|| fail(StatusCode::BAD_REQUEST, "an anchored chat needs `line`"))?;
                    let side = if req.side.as_deref() == Some("LEFT") { Side::Left } else { Side::Right };
                    Some(Anchor { path: path.clone(), start: req.start.filter(|&s| s != line), line, side })
                }
                None => None,
            };
            let name = match &anchor {
                Some(a) => format!("review-master: chat {}", a.location()),
                None => "review-master: chat".to_owned(),
            };
            let session = st.engine.start_session(&name).await.map_err(|e| fail(StatusCode::BAD_GATEWAY, e))?;
            let prompt = chat_prompt(&st, anchor.as_ref(), req.snippet.as_deref(), &req.question);
            let mut threads = st.threads.lock().unwrap();
            threads.next += 1;
            let id = threads.next;
            threads.by_id.insert(
                id,
                Thread { id, anchor, session: session.clone(), messages: vec![("user".to_owned(), req.question.clone())], busy: true, job: None },
            );
            (id, session, prompt)
        }
    };
    let job = new_job(&mut st.jobs.lock().unwrap());
    let job_id = job.id;
    {
        let mut threads = st.threads.lock().unwrap();
        if let Some(t) = threads.by_id.get_mut(&thread_id) {
            t.job = Some(job_id);
        }
        threads.save();
    }
    let st2 = st.clone();
    tokio::spawn(async move {
        let progress = |line: String| job.push(line);
        let out = st2.engine.turn::<ChatAnswer>(&session, &prompt, &chat_schema(), &job.cancel, progress).await;
        let mut threads = st2.threads.lock().unwrap();
        let thread = threads.by_id.get_mut(&thread_id);
        let result = out.map(|out| {
            let answer = out.answer.answer;
            if let Some(t) = thread {
                t.messages.push(("assistant".to_owned(), answer.clone()));
            }
            json!({ "answer": answer })
        });
        if let Some(t) = threads.by_id.get_mut(&thread_id) {
            t.busy = false;
            t.job = None;
        }
        threads.save();
        drop(threads);
        job.finish(result);
    });
    Ok(Json(json!({ "thread": thread_id, "job": job_id })))
}

#[derive(Deserialize)]
struct ThreadQuery {
    id: u64,
}

async fn thread(State(st): State<Arc<AppState>>, Query(q): Query<ThreadQuery>) -> ApiResult {
    let threads = st.threads.lock().unwrap();
    let t = threads.by_id.get(&q.id).ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no thread {}", q.id)))?;
    let messages: Vec<Value> = t.messages.iter().map(|(role, text)| json!({ "role": role, "text": text })).collect();
    Ok(Json(json!({
        "id": t.id,
        "anchor": t.anchor.as_ref().map(Anchor::json),
        "messages": messages,
        "busy": t.busy,
        "job": t.job,
    })))
}

async fn delete_thread(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<u64>) -> ApiResult {
    let mut threads = st.threads.lock().unwrap();
    match threads.by_id.get(&id) {
        None => return Err(fail(StatusCode::NOT_FOUND, format!("no thread {id}"))),
        Some(t) if t.busy => return Err(fail(StatusCode::CONFLICT, "this thread is still answering; stop it first")),
        Some(_) => {}
    }
    threads.by_id.remove(&id);
    threads.save();
    Ok(Json(json!({})))
}

async fn threads(State(st): State<Arc<AppState>>) -> Json<Value> {
    let threads = st.threads.lock().unwrap();
    let mut list: Vec<&Thread> = threads.by_id.values().collect();
    list.sort_by_key(|t| t.id);
    let out: Vec<Value> = list
        .into_iter()
        .map(|t| {
            let last = t.messages.last().map(|(_, text)| crate::engine::one_line(text, 80)).unwrap_or_default();
            json!({ "id": t.id, "anchor": t.anchor.as_ref().map(Anchor::json), "messages": t.messages.len(), "last": last })
        })
        .collect();
    Json(json!(out))
}

#[derive(Deserialize)]
struct SinceQuery {
    #[serde(default)]
    since: usize,
}

async fn job_status(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<u64>, Query(q): Query<SinceQuery>) -> ApiResult {
    let jobs = st.jobs.lock().unwrap();
    let job = jobs.by_id.get(&id).ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no job {id}")))?;
    let inner = job.inner.lock().unwrap();
    let since = q.since.min(inner.progress.len());
    Ok(Json(json!({
        "done": inner.done,
        "progress": inner.progress[since..],
        "next": inner.progress.len(),
        "error": inner.error,
        "result": inner.result,
    })))
}

/// Stop a running engine turn; its job then finishes with the error `cancelled`.
async fn cancel_job(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<u64>) -> ApiResult {
    let jobs = st.jobs.lock().unwrap();
    let job = jobs.by_id.get(&id).ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no job {id}")))?;
    job.cancel.notify_one();
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct CommentsQuery {
    #[serde(default)]
    refresh: bool,
}

/// Review comments already on the pull request (none in local mode).
async fn pr_comments(State(st): State<Arc<AppState>>, Query(q): Query<CommentsQuery>) -> ApiResult {
    let Some((r, _)) = &st.pr else { return Ok(Json(json!({ "comments": [] }))) };
    let cached = if q.refresh { None } else { st.comments.lock().unwrap().clone() };
    let list = match cached {
        Some(list) => list,
        None => {
            let pref = PrRef { owner: r.owner.clone(), repo: r.repo.clone(), number: r.number };
            let list = tokio::task::spawn_blocking(move || crate::pr::review_comments(&pref))
                .await
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .map_err(|e| fail(StatusCode::BAD_GATEWAY, e))?;
            *st.comments.lock().unwrap() = Some(list.clone());
            list
        }
    };
    Ok(Json(json!({ "comments": list })))
}

#[derive(Deserialize)]
struct ReplyRequest {
    /// The thread's first comment.
    id: u64,
    body: String,
}

/// Reply in an existing review thread; unlike drafts this is posted right away.
async fn reply_comment(State(st): State<Arc<AppState>>, Json(req): Json<ReplyRequest>) -> ApiResult {
    let Some((r, _)) = &st.pr else { return Err(fail(StatusCode::BAD_REQUEST, "no pull request to reply on")) };
    if req.body.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "the reply is empty"));
    }
    let pref = PrRef { owner: r.owner.clone(), repo: r.repo.clone(), number: r.number };
    tokio::task::spawn_blocking(move || crate::pr::reply(&pref, req.id, &req.body))
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| fail(StatusCode::BAD_GATEWAY, e))?;
    *st.comments.lock().unwrap() = None; // the next listing fetches the reply too
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct DraftRequest {
    #[serde(default)]
    id: Option<u64>,
    path: String,
    line: u64,
    #[serde(default)]
    start: Option<u64>,
    side: String,
    severity: String,
    body: String,
}

async fn upsert_draft(State(st): State<Arc<AppState>>, Json(req): Json<DraftRequest>) -> ApiResult {
    if req.body.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "the comment is empty"));
    }
    let side = if req.side == "LEFT" { Side::Left } else { Side::Right };
    let start = req.start.filter(|&s| s != req.line);
    let (start, line) = match start {
        Some(s) if s > req.line => (Some(req.line), s),
        other => (other, req.line),
    };
    let problem = st.scope.lock().unwrap().anchors.check(&req.path, side, start, line).err();
    let mut drafts = st.drafts.lock().unwrap();
    let id = match req.id {
        Some(id) => {
            let d = drafts.items.iter_mut().find(|d| d.id == id).ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no draft {id}")))?;
            *d = Draft { id, path: req.path, line, start, side: side.api().to_owned(), severity: req.severity, body: req.body, problem: problem.clone() };
            id
        }
        None => {
            drafts.next += 1;
            let id = drafts.next;
            drafts.items.push(Draft { id, path: req.path, line, start, side: side.api().to_owned(), severity: req.severity, body: req.body, problem: problem.clone() });
            id
        }
    };
    drafts.save();
    Ok(Json(json!({ "id": id, "problem": problem })))
}

async fn drafts(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(json!(st.drafts.lock().unwrap().items))
}

async fn delete_draft(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<u64>) -> ApiResult {
    let mut drafts = st.drafts.lock().unwrap();
    let before = drafts.items.len();
    drafts.items.retain(|d| d.id != id);
    if drafts.items.len() == before {
        return Err(fail(StatusCode::NOT_FOUND, format!("no draft {id}")));
    }
    drafts.save();
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct SubmitRequest {
    event: String,
    #[serde(default)]
    body: String,
}

async fn submit(State(st): State<Arc<AppState>>, Json(req): Json<SubmitRequest>) -> ApiResult {
    let Some((pref, _)) = &st.pr else {
        return Err(fail(StatusCode::BAD_REQUEST, "no pull request: reviewing a local worktree; give the PR on the command line to submit"));
    };
    let event = match req.event.as_str() {
        "comment" => post::Event::Comment,
        "approve" => post::Event::Approve,
        "request_changes" => post::Event::RequestChanges,
        other => return Err(fail(StatusCode::BAD_REQUEST, format!("unknown event `{other}`"))),
    };
    if event == post::Event::RequestChanges && req.body.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "request_changes needs a summary"));
    }
    let items = st.drafts.lock().unwrap().items.clone();
    if items.is_empty() && req.body.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "nothing to submit: no drafts and no summary"));
    }
    let mut inline = Vec::new();
    let mut fallback = Vec::new();
    let mut excluded_drafts = 0usize;
    {
        let scope = st.scope.lock().unwrap();
        for d in items {
            // Drafts on files excluded from this review are kept but never posted.
            if scope.model.file(&d.path).is_none() {
                excluded_drafts += 1;
                continue;
            }
            let finding = Finding {
                path: d.path,
                line: d.line,
                start_line: d.start,
                side: Some(d.side),
                severity: d.severity,
                title: None,
                body: d.body,
            };
            match review::anchor(&scope.anchors, &finding) {
                Ok(a) => inline.push(a),
                Err(bad) => fallback.push(bad),
            }
        }
    }
    if inline.is_empty() && fallback.is_empty() && req.body.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, format!("nothing to submit: {excluded_drafts} draft(s) sit on files excluded from this review")));
    }
    let payload = post::payload(&st.head_sha, event, &req.body, &inline, &fallback);
    let pref = PrRef { owner: pref.owner.clone(), repo: pref.repo.clone(), number: pref.number };
    let url = tokio::task::spawn_blocking(move || post::submit(&pref, &payload))
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| fail(StatusCode::BAD_GATEWAY, e))?;
    *st.submitted.lock().unwrap() = Some(url.clone());
    // Submitted drafts are done; drafts on excluded files survive for a later review.
    {
        let mut drafts = st.drafts.lock().unwrap();
        let scope = st.scope.lock().unwrap();
        drafts.items.retain(|d| scope.model.file(&d.path).is_none());
        drafts.save();
    }
    Ok(Json(json!({ "url": url, "not_submitted": excluded_drafts })))
}

#[derive(Deserialize)]
struct ScopeRequest {
    excluded: Vec<String>,
}

/// Replace the set of files excluded from this review; persisted across restarts.
async fn set_scope(State(st): State<Arc<AppState>>, Json(req): Json<ScopeRequest>) -> ApiResult {
    let mut excluded: Vec<String> = req.excluded.into_iter().filter(|p| st.full.file(p).is_some()).collect();
    excluded.sort();
    excluded.dedup();
    if excluded.len() == st.full.files.len() {
        return Err(fail(StatusCode::BAD_REQUEST, "cannot exclude every file in the review"));
    }
    let scope = build_scope(&st.full, excluded);
    st.save_scope(&scope.excluded);
    let response = json!({ "files": files_json(&scope.model), "skipped": st.cli_skipped, "excluded": scope.excluded });
    *st.scope.lock().unwrap() = scope;
    Ok(Json(response))
}

// ---- summary ----------------------------------------------------------------

#[derive(Default)]
struct SummaryState {
    text: Option<String>,
    job: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredSummary {
    head: String,
    summary: String,
}

/// Diff budget for whole-change prompts (summary, flows); files beyond it are listed instead.
const MAX_PROMPT_DIFF_BYTES: usize = 384 * 1024;

fn summary_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary"],
        "properties": { "summary": { "type": "string", "minLength": 1, "description": "the Markdown summary" } }
    })
}

#[derive(Deserialize)]
struct SummaryAnswer {
    summary: String,
}

/// What is being reviewed, with the PR description: the opening of whole-change prompts.
fn subject_block(st: &AppState) -> String {
    let subject = st.subject();
    let mut p = subject.heading;
    let description: String = subject.description.trim().chars().take(4000).collect();
    if !description.is_empty() {
        p.push_str(&format!("\n<description>\n{description}\n</description>\n"));
    }
    p
}

/// The whole change's diff for a prompt, within the budget.
fn diff_block(model: &Model) -> String {
    let mut diff_text = String::new();
    let mut deferred: Vec<&str> = Vec::new();
    for file in &model.files {
        if diff_text.len() < MAX_PROMPT_DIFF_BYTES {
            Model::render_file(file, &mut diff_text);
        } else {
            deferred.push(&file.path);
        }
    }
    let mut p = format!("\n<diff>\n{diff_text}</diff>\n");
    if !deferred.is_empty() {
        p.push_str(&format!("\nAlso changed, diff omitted for size (read them if relevant): {}\n", deferred.join(", ")));
    }
    p
}

fn summary_prompt(st: &AppState) -> String {
    let mut p = subject_block(st);
    p.push_str(
        "\nWrite the summary a reviewer should read before the diff. Markdown, exactly these sections:\n\
## Why the change\nExactly one sentence: the problem this solves and what becomes possible.\n\
## Special things to note\n1-3 bullets a reviewer must know — warnings, migrations, compatibility \
constraints, deliberate omissions, surprising decisions. `- None.` when there are none.\n\
## Change outline\nThe smallest set of structural views that explains the implementation: a shallow file \
tree with changed responsibilities, key new or changed types and contracts, concise pseudocode or \
control/data flow for changed behavior. Fenced code blocks; `diff` blocks for changes to existing shapes. \
Cite `path:line` anchors so they are clickable. No file-by-file changelog, no praise, plain language, \
written as one human explaining to another.\n\
Investigate with the repository tools when the diff alone is not enough; verify cited line numbers by \
reading the files.\n",
    );
    p.push_str(&diff_block(&st.full));
    p
}

async fn get_summary(State(st): State<Arc<AppState>>) -> ApiResult {
    {
        let s = st.summary.lock().unwrap();
        if let Some(text) = &s.text {
            return Ok(Json(json!({ "summary": text, "job": null })));
        }
        if let Some(job) = s.job {
            let running = st.jobs.lock().unwrap().by_id.get(&job).is_some_and(|j| !j.inner.lock().unwrap().done);
            if running {
                return Ok(Json(json!({ "summary": null, "job": job })));
            }
        }
    }
    spawn_summary(st)
}

/// Force a fresh summary.
async fn regenerate_summary(State(st): State<Arc<AppState>>) -> ApiResult {
    st.summary.lock().unwrap().text = None;
    spawn_summary(st)
}

fn spawn_summary(st: Arc<AppState>) -> ApiResult {
    let job = new_job(&mut st.jobs.lock().unwrap());
    st.summary.lock().unwrap().job = Some(job.id);
    let job_id = job.id;
    let name = match &st.pr {
        Some((r, _)) => format!("review-master: summary {}#{}", r.slug(), r.number),
        None => format!("review-master: summary {}", st.branch),
    };
    let st2 = st.clone();
    tokio::spawn(async move {
        let prompt = summary_prompt(&st2);
        let result = async {
            let session = st2.engine.start_session(&name).await?;
            let progress = |line: String| job.push(line);
            let out = st2.engine.turn::<SummaryAnswer>(&session, &prompt, &summary_schema(), &job.cancel, progress).await?;
            Ok::<String, String>(out.answer.summary)
        }
        .await;
        match result {
            Ok(summary) => {
                {
                    let mut s = st2.summary.lock().unwrap();
                    s.text = Some(summary.clone());
                    s.job = None;
                }
                let stored = StoredSummary { head: st2.head_sha.clone(), summary: summary.clone() };
                if let Ok(bytes) = serde_json::to_vec_pretty(&stored) {
                    let _ = fs::write(&st2.summary_file, bytes);
                }
                job.finish(Ok(json!({ "summary": summary })));
            }
            Err(e) => {
                st2.summary.lock().unwrap().job = None;
                job.finish(Err(e));
            }
        }
    });
    Ok(Json(json!({ "summary": null, "job": job_id })))
}

// ---- flows ------------------------------------------------------------------

#[derive(Default)]
struct FlowsState {
    flows: Option<flows::Flows>,
    job: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredFlows {
    head: String,
    version: u32,
    flows: flows::Flows,
}

fn job_running(st: &AppState, id: u64) -> bool {
    st.jobs.lock().unwrap().by_id.get(&id).is_some_and(|j| !j.inner.lock().unwrap().done)
}

/// The flows of this change, cached per head commit; generated only on request.
async fn get_flows(State(st): State<Arc<AppState>>) -> Json<Value> {
    let f = st.flows.lock().unwrap();
    let job = f.job.filter(|&id| job_running(&st, id));
    Json(json!({ "flows": f.flows.as_ref().map(|x| &x.flows), "job": job }))
}

/// Generate the flows afresh; a generation already running is returned instead.
async fn generate_flows(State(st): State<Arc<AppState>>) -> ApiResult {
    if let Some(job) = st.flows.lock().unwrap().job.filter(|&id| job_running(&st, id)) {
        return Ok(Json(json!({ "job": job })));
    }
    let job = new_job(&mut st.jobs.lock().unwrap());
    st.flows.lock().unwrap().job = Some(job.id);
    let job_id = job.id;
    let name = match &st.pr {
        Some((r, _)) => format!("review-master: flows {}#{}", r.slug(), r.number),
        None => format!("review-master: flows {}", st.branch),
    };
    let st2 = st.clone();
    tokio::spawn(async move {
        let prompt = flows::prompt(&subject_block(&st2), &diff_block(&st2.full));
        let result = async {
            let session = st2.engine.start_session(&name).await?;
            let progress = |line: String| job.push(line);
            let out = st2.engine.turn::<flows::Flows>(&session, &prompt, &flows::schema(), &job.cancel, progress).await?;
            Ok::<flows::Flows, String>(out.answer)
        }
        .await;
        match result {
            Ok(mut found) => {
                flows::verify(&st2.root, &st2.full, &mut found);
                let stored = StoredFlows { head: st2.head_sha.clone(), version: flows::VERSION, flows: found.clone() };
                if let Ok(bytes) = serde_json::to_vec_pretty(&stored) {
                    let _ = fs::write(&st2.flows_file, bytes);
                }
                let response = json!({ "flows": found.flows });
                {
                    let mut f = st2.flows.lock().unwrap();
                    f.flows = Some(found);
                    f.job = None;
                }
                job.finish(Ok(response));
            }
            Err(e) => {
                st2.flows.lock().unwrap().job = None;
                job.finish(Err(e));
            }
        }
    });
    Ok(Json(json!({ "job": job_id })))
}

// ---- imported findings ------------------------------------------------------

/// Every finding, with why it could not become an inline comment (`anchor_problem`),
/// plus the imported documents in import order.
fn findings_json(st: &AppState) -> Value {
    let store = st.findings.lock().unwrap();
    let scope = st.scope.lock().unwrap();
    let items: Vec<Value> = store
        .items
        .iter()
        .map(|f| {
            let side = if f.side == "LEFT" { Side::Left } else { Side::Right };
            let mut v = json!(f);
            v["anchor_problem"] = json!(scope.anchors.check(&f.path, side, f.start, f.line).err());
            v
        })
        .collect();
    let mut sources: Vec<&str> = Vec::new();
    for f in &store.items {
        if !sources.contains(&f.source.as_str()) {
            sources.push(&f.source);
        }
    }
    json!({ "findings": items, "sources": sources })
}

async fn list_findings(State(st): State<Arc<AppState>>) -> Json<Value> {
    Json(findings_json(&st))
}

#[derive(Deserialize)]
struct ImportRequest {
    /// File name of the document; an import of the same name replaces the earlier one.
    #[serde(default)]
    name: String,
    text: String,
}

async fn import_findings(State(st): State<Arc<AppState>>, Json(req): Json<ImportRequest>) -> ApiResult {
    if req.text.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "the document is empty"));
    }
    let base = req.name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let name: String = if base.is_empty() { "pasted.md".to_owned() } else { base.chars().take(120).collect() };
    let imported = st.import_findings(&name, &req.text);
    let mut out = findings_json(&st);
    out["imported"] = json!({ "source": name, "added": imported.added, "skipped": imported.skipped });
    Ok(Json(out))
}

#[derive(Deserialize)]
struct FindingStatusRequest {
    status: String,
}

/// Dismiss a finding, mark it as turned into a draft, or reopen it.
async fn set_finding_status(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<u64>, Json(req): Json<FindingStatusRequest>) -> ApiResult {
    {
        let mut store = st.findings.lock().unwrap();
        store.set_status(id, &req.status).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        store.save();
    }
    Ok(Json(findings_json(&st)))
}

#[derive(Deserialize)]
struct SourceQuery {
    #[serde(default)]
    source: Option<String>,
}

/// Forget one imported document's findings, or all of them.
async fn remove_findings(State(st): State<Arc<AppState>>, Query(q): Query<SourceQuery>) -> Json<Value> {
    {
        let mut store = st.findings.lock().unwrap();
        store.remove(q.source.as_deref());
        store.save();
    }
    Json(findings_json(&st))
}

// ---- test folds -------------------------------------------------------------

/// Whether any of `lines[from..=to]` is a `#[cfg(…test…)]` gate (not `not(test)`).
fn is_test_gated(lines: &[&str], from: usize, to: usize) -> bool {
    let to = to.min(lines.len().saturating_sub(1));
    lines.get(from..=to).into_iter().flatten().any(|l| {
        let t = l.trim_start();
        t.starts_with("#[cfg(") && t.contains("test") && !t.contains("not(test")
    })
}

/// Walk LSP document symbols; an item whose leading attribute lines carry
/// `#[cfg(test)]` folds whole (children skipped). Lines are 0-based in LSP.
fn gated_ranges(symbols: &[Value], lines: &[&str], out: &mut Vec<(u64, u64)>) {
    for s in symbols {
        let (Some(start), Some(end)) = (s["range"]["start"]["line"].as_u64(), s["range"]["end"]["line"].as_u64()) else { continue };
        // Attributes sit between the item's range start and its name.
        let name_line = s["selectionRange"]["start"]["line"].as_u64().unwrap_or(start);
        if is_test_gated(lines, start as usize, name_line as usize) {
            out.push((start + 1, end + 1));
            continue;
        }
        if let Some(children) = s["children"].as_array() {
            gated_ranges(children, lines, out);
        }
    }
}

/// Text fallback when no language server answers: match the attribute, then
/// brace-count (string/comment aware, good enough for folding) to the close.
fn rust_test_folds_text(text: &str) -> Vec<(u64, u64)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut folds = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim_start();
        if t.starts_with("#[cfg(") && t.contains("test") && !t.contains("not(test") {
            let start = i as u64 + 1;
            let mut depth = 0i64;
            let mut opened = false;
            let mut in_block_comment = false;
            let mut j = i;
            'scan: while j < lines.len() {
                let bytes = lines[j].as_bytes();
                let (mut in_str, mut in_char) = (false, false);
                let mut k = 0usize;
                while k < bytes.len() {
                    let c = bytes[k];
                    if in_block_comment {
                        if c == b'*' && bytes.get(k + 1) == Some(&b'/') {
                            in_block_comment = false;
                            k += 1;
                        }
                    } else if in_str {
                        if c == b'\\' {
                            k += 1;
                        } else if c == b'"' {
                            in_str = false;
                        }
                    } else if in_char {
                        if c == b'\\' {
                            k += 1;
                        } else if c == b'\'' {
                            in_char = false;
                        }
                    } else {
                        match c {
                            b'/' if bytes.get(k + 1) == Some(&b'/') => break,
                            b'/' if bytes.get(k + 1) == Some(&b'*') => {
                                in_block_comment = true;
                                k += 1;
                            }
                            b'"' => in_str = true,
                            b'\'' if bytes.get(k + 1) == Some(&b'\\') || bytes.get(k + 2) == Some(&b'\'') => in_char = true,
                            b'{' => {
                                depth += 1;
                                opened = true;
                            }
                            b'}' => {
                                depth -= 1;
                                if opened && depth == 0 {
                                    folds.push((start, j as u64 + 1));
                                    i = j;
                                    break 'scan;
                                }
                            }
                            b';' if !opened && depth == 0 => break 'scan,
                            _ => {}
                        }
                    }
                    k += 1;
                }
                j += 1;
            }
        }
        i += 1;
    }
    folds
}

/// `#[cfg(test)]` fold ranges for a Rust file: language-server boundaries when
/// available, text scanning otherwise.
async fn folds(State(st): State<Arc<AppState>>, Query(q): Query<PathQuery>) -> ApiResult {
    if lang(&q.path) != "rs" {
        return Ok(Json(json!({ "test_folds": [], "source": "text" })));
    }
    let (full, shown) = verify::resolve(&st.root, &q.path).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
    let meta = fs::metadata(&full).map_err(|e| fail(StatusCode::NOT_FOUND, e.to_string()))?;
    if !meta.is_file() || meta.len() > MAX_SOURCE_BYTES {
        return Ok(Json(json!({ "test_folds": [], "source": "text" })));
    }
    let bytes = fs::read(&full).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let mut ranges = Vec::new();
    let mut source = "lsp";
    match st.lsp.document_symbols(&full, &shown).await {
        Ok(symbols) => {
            let lines: Vec<&str> = text.lines().collect();
            gated_ranges(&symbols, &lines, &mut ranges);
        }
        Err(_) => {
            source = "text";
            ranges = rust_test_folds_text(&text);
        }
    }
    ranges.sort_unstable();
    let test_folds: Vec<Value> = ranges.iter().map(|(s, e)| json!([s, e])).collect();
    Ok(Json(json!({ "test_folds": test_folds, "source": source })))
}
