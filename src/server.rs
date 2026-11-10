//! Local HTTP API and embedded web UI: browse the change, chat with the engine
//! about exact lines, keep draft comments, submit one GitHub review. Engine
//! turns run as background jobs the UI polls.

use crate::diff::{Diff, Kind, Model, Side, Status};
use crate::engine::Engine;
use crate::pr::{PrMeta, PrRef};
use crate::review::{self, Finding};
use crate::{post, verify};
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
    pub model: Model,
    pub anchors: Diff,
    /// Files dropped from the review (excluded or over budget).
    pub skipped: Vec<String>,
    pub engine: Engine,
    /// The `gh` login, for own-PR hints in the UI.
    pub viewer: Option<String>,
    drafts: Mutex<DraftStore>,
    threads: Mutex<Threads>,
    jobs: Mutex<Jobs>,
    submitted: Mutex<Option<String>>,
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
    ) -> Arc<AppState> {
        let anchors = model.anchors();
        Arc::new(AppState {
            root,
            pr,
            head_sha,
            branch,
            dirty,
            model,
            anchors,
            skipped,
            engine,
            viewer,
            drafts: Mutex::new(DraftStore::load(drafts_file)),
            threads: Mutex::default(),
            jobs: Mutex::default(),
            submitted: Mutex::new(None),
        })
    }

    fn subject(&self) -> review::Subject {
        match &self.pr {
            Some((r, m)) => review::Subject::pr(r, m, &self.head_sha),
            None => review::Subject::local(&self.branch, &self.head_sha, self.dirty),
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

#[derive(Clone)]
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

struct Thread {
    id: u64,
    anchor: Option<Anchor>,
    session: String,
    /// (role, text)
    messages: Vec<(&'static str, String)>,
    busy: bool,
}

#[derive(Default)]
struct Threads {
    next: u64,
    by_id: HashMap<u64, Thread>,
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
        .route("/api/threads", get(threads))
        .route("/api/job/{id}", get(job_status))
        .route("/api/draft", post_to(upsert_draft))
        .route("/api/draft/{id}", delete(delete_draft))
        .route("/api/drafts", get(drafts))
        .route("/api/submit", post_to(submit))
        .with_state(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

async fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

// ---- handlers ---------------------------------------------------------------

async fn info(State(st): State<Arc<AppState>>) -> Json<Value> {
    let files: Vec<Value> = st
        .model
        .files
        .iter()
        .map(|f| {
            json!({
                "path": f.path,
                "status": f.status.api(),
                "old_path": f.old_path,
                "additions": f.additions,
                "deletions": f.deletions,
            })
        })
        .collect();
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
        "skipped": st.skipped,
        "submitted": *st.submitted.lock().unwrap(),
    }))
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
}

async fn file_diff(State(st): State<Arc<AppState>>, Query(q): Query<PathQuery>) -> ApiResult {
    let file = st.model.file(&q.path).ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("`{}` is not part of the change", q.path)))?;
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
    Ok(Json(json!({ "path": file.path, "hunks": hunks })))
}

/// Language hint for the UI: the lowercased file extension.
fn lang(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path).rsplit_once('.').map_or("", |(_, ext)| ext)
}

async fn source(State(st): State<Arc<AppState>>, Query(q): Query<PathQuery>) -> ApiResult {
    if st.model.file(&q.path).is_some_and(|f| f.status == Status::Deleted) {
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
    if let Some(file) = st.model.file(&q.path) {
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
fn anchored_hunks(st: &AppState, a: &Anchor) -> String {
    let Some(file) = st.model.file(&a.path) else { return String::new() };
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
fn chat_prompt(st: &AppState, anchor: Option<&Anchor>, question: &str) -> String {
    let subject = st.subject();
    let mut p = subject.heading;
    let description: String = subject.description.trim().chars().take(4000).collect();
    if !description.is_empty() {
        p.push_str(&format!("\n<description>\n{description}\n</description>\n"));
    }
    match anchor {
        Some(a) => {
            p.push_str(&format!("\nThe reviewer selected {} ({} side of the diff).\n", a.location(), a.side.api()));
            let hunks = anchored_hunks(st, a);
            if !hunks.is_empty() {
                p.push_str(&format!("\n<diff-hunk>\n{hunks}</diff-hunk>\n"));
            }
            let window = file_window(st, a);
            if !window.is_empty() {
                p.push_str(&format!("\n<head-file-excerpt>\n{window}</head-file-excerpt>\n"));
            }
        }
        None => {
            let list: Vec<String> = st.model.files.iter().map(|f| format!("{} ({})", f.path, f.status.api())).collect();
            p.push_str(&format!("\nChanged files: {}\n", list.join(", ")));
        }
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
            t.messages.push(("user", req.question.clone()));
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
            let prompt = chat_prompt(&st, anchor.as_ref(), &req.question);
            let mut threads = st.threads.lock().unwrap();
            threads.next += 1;
            let id = threads.next;
            threads.by_id.insert(
                id,
                Thread { id, anchor, session: session.clone(), messages: vec![("user", req.question.clone())], busy: true },
            );
            (id, session, prompt)
        }
    };
    let job = new_job(&mut st.jobs.lock().unwrap());
    let job_id = job.id;
    let st2 = st.clone();
    tokio::spawn(async move {
        let progress = |line: String| job.push(line);
        let out = st2.engine.turn::<ChatAnswer>(&session, &prompt, &chat_schema(), &job.cancel, progress).await;
        let result = match out {
            Ok(out) => {
                let answer = out.answer.answer;
                let mut threads = st2.threads.lock().unwrap();
                if let Some(t) = threads.by_id.get_mut(&thread_id) {
                    t.messages.push(("assistant", answer.clone()));
                    t.busy = false;
                }
                Ok(json!({ "answer": answer }))
            }
            Err(e) => {
                let mut threads = st2.threads.lock().unwrap();
                if let Some(t) = threads.by_id.get_mut(&thread_id) {
                    t.busy = false;
                }
                Err(e)
            }
        };
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
    })))
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
    let problem = st.anchors.check(&req.path, side, start, line).err();
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
    for d in items {
        let finding = Finding {
            path: d.path,
            line: d.line,
            start_line: d.start,
            side: Some(d.side),
            severity: d.severity,
            title: None,
            body: d.body,
        };
        match review::anchor(&st.anchors, &finding) {
            Ok(a) => inline.push(a),
            Err(bad) => fallback.push(bad),
        }
    }
    let payload = post::payload(&st.head_sha, event, &req.body, &inline, &fallback);
    let pref = PrRef { owner: pref.owner.clone(), repo: pref.repo.clone(), number: pref.number };
    let url = tokio::task::spawn_blocking(move || post::submit(&pref, &payload))
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| fail(StatusCode::BAD_GATEWAY, e))?;
    *st.submitted.lock().unwrap() = Some(url.clone());
    // Submitted drafts are done; clear them so a follow-up review starts clean.
    {
        let mut drafts = st.drafts.lock().unwrap();
        drafts.items.clear();
        drafts.save();
    }
    Ok(Json(json!({ "url": url })))
}
