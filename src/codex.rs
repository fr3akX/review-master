//! Client for `codex app-server` (JSON-RPC over stdio, one message per line).
//!
//! Sessions are persisted Codex threads, read-only, that never ask for approval
//! and are named `review-master: …` in the Codex session list.

use crate::engine::{one_line, Outcome, SESSION_GONE};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Notify};

/// Integrations an explorer never needs; disabling them keeps user hooks, plugins
/// and browser/computer tools out of sessions over untrusted repositories.
const DISABLED_FEATURES: &[&str] =
    &["hooks", "plugins", "apps", "memories", "browser_use", "computer_use", "in_app_browser"];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const STDERR_TAIL: usize = 8 * 1024;

const DEVELOPER_INSTRUCTIONS: &str = "\
You are review-master's read-only pull-request review agent. The pull request's checkout is your working \
directory. Use only read-only commands (ls, rg, sed -n, nl -ba, cat, git log, git show, \
git diff). Never modify files, install packages, run the project's code, builds or tests, \
or use the network. When a turn defines an output schema, answer with exactly that JSON.";

pub struct CodexConfig {
    pub bin: String,
    pub model: Option<String>,
    /// Limit for one turn.
    pub timeout: Duration,
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>;

pub struct Codex {
    cfg: CodexConfig,
    cwd: PathBuf,
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    /// Notifications of the turn running in a thread.
    subscribers: Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>,
    /// Threads started, forked or resumed by this process.
    loaded: Mutex<HashSet<String>>,
    exited: Mutex<Option<String>>,
    _child: Child,
}

/// Removes a turn's subscription however the turn ends.
struct Subscription<'a> {
    codex: &'a Codex,
    thread: String,
}

impl Drop for Subscription<'_> {
    fn drop(&mut self) {
        self.codex.subscribers.lock().unwrap().remove(&self.thread);
    }
}

impl Codex {
    /// Start `codex app-server` for a repository and complete the handshake.
    pub async fn start(cfg: CodexConfig, cwd: &Path) -> Result<Arc<Codex>, String> {
        let mut cmd = Command::new(&cfg.bin);
        cmd.arg("app-server");
        for feature in DISABLED_FEATURES {
            cmd.args(["--disable", feature]);
        }
        let mut child = cmd
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start `{} app-server`: {e}", cfg.bin))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");

        let codex = Arc::new(Codex {
            cfg,
            cwd: cwd.to_owned(),
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::default(),
            next_id: AtomicU64::new(1),
            subscribers: Mutex::default(),
            loaded: Mutex::default(),
            exited: Mutex::default(),
            _child: child,
        });

        let tail = Arc::new(Mutex::new(String::new()));
        let tail_w = tail.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut chunk).await {
                let mut t = tail_w.lock().unwrap();
                t.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if t.len() > STDERR_TAIL {
                    let mut cut = t.len() - STDERR_TAIL;
                    while !t.is_char_boundary(cut) {
                        cut += 1;
                    }
                    t.drain(..cut);
                }
            }
        });

        // The reader holds a weak reference so dropping review-master's handle stops the server.
        let weak = Arc::downgrade(&codex);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(codex) = weak.upgrade() else { return };
                codex.dispatch(&line).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Some(codex) = weak.upgrade() {
                let t = tail.lock().unwrap().clone();
                let why = t.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output").trim().to_owned();
                codex.fail_all(format!("codex app-server exited: {why}"));
            }
        });

        let info = json!({ "clientInfo": { "name": "review-master", "version": env!("CARGO_PKG_VERSION") } });
        codex.request("initialize", info).await.map_err(|e| {
            format!("`{} app-server` did not initialize ({e}); review-master needs a Codex CLI with app-server support", codex.cfg.bin)
        })?;
        codex.notify("initialized", json!({})).await?;
        Ok(codex)
    }

    pub fn model(&self) -> Option<&str> {
        self.cfg.model.as_deref()
    }

    fn fail_all(&self, reason: String) {
        *self.exited.lock().unwrap() = Some(reason.clone());
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(reason.clone()));
        }
        self.subscribers.lock().unwrap().clear();
    }

    async fn write(&self, msg: &Value) -> Result<(), String> {
        if let Some(reason) = self.exited.lock().unwrap().clone() {
            return Err(reason);
        }
        let mut line = serde_json::to_string(msg).map_err(|e| e.to_string())?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await.map_err(|e| format!("codex app-server: {e}"))?;
        stdin.flush().await.map_err(|e| format!("codex app-server: {e}"))
    }

    async fn dispatch(&self, line: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { return };
        let method = msg["method"].as_str();
        match (method, msg.get("id")) {
            (None, Some(id)) => {
                let tx = id.as_u64().and_then(|id| self.pending.lock().unwrap().remove(&id));
                if let Some(tx) = tx {
                    let result = match msg.get("error") {
                        Some(err) => Err(err["message"].as_str().unwrap_or("request failed").to_owned()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(result);
                }
            }
            // Sessions are read-only with approvals off; anything asking anyway is declined.
            (Some(method), Some(id)) => {
                let reply = match method {
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                        json!({ "jsonrpc": "2.0", "id": id, "result": { "decision": "decline" } })
                    }
                    _ => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "not supported by review-master" } }),
                };
                let _ = self.write(&reply).await;
            }
            (Some(_), None) => {
                let thread = msg["params"]["threadId"].as_str().unwrap_or_default();
                if let Some(tx) = self.subscribers.lock().unwrap().get(thread) {
                    let _ = tx.send(msg);
                }
            }
            _ => {}
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result.map_err(|e| format!("{method}: {e}")),
            Ok(Err(_)) => Err(self.exited.lock().unwrap().clone().unwrap_or_else(|| "codex app-server stopped".into())),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("{method} timed out"))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    /// Settings every review-master session runs with.
    fn session_params(&self) -> Value {
        let mut p = json!({
            "cwd": self.cwd,
            "sandbox": "read-only",
            "approvalPolicy": "never",
            "ephemeral": false,
            "developerInstructions": DEVELOPER_INSTRUCTIONS,
        });
        if let Some(model) = &self.cfg.model {
            p["model"] = json!(model);
        }
        p
    }

    async fn named(&self, result: Value, name: &str) -> Result<String, String> {
        let id = result["thread"]["id"].as_str().ok_or("codex returned no thread id")?.to_owned();
        self.loaded.lock().unwrap().insert(id.clone());
        // A missing name is cosmetic; the session works without it.
        let _ = self.request("thread/name/set", json!({ "threadId": id, "name": name })).await;
        Ok(id)
    }

    /// A new persisted session.
    pub async fn start_thread(&self, name: &str) -> Result<String, String> {
        let result = self.request("thread/start", self.session_params()).await?;
        self.named(result, name).await
    }

    /// A new persisted session that starts with everything `parent` has seen.
    pub async fn fork(&self, parent: &str, name: &str) -> Result<String, String> {
        let mut params = self.session_params();
        params["threadId"] = json!(parent);
        let result = self.request("thread/fork", params).await?;
        self.named(result, name).await
    }

    /// Load a session created earlier (possibly by another review-master run) before using it.
    async fn ensure_loaded(&self, thread: &str) -> Result<(), String> {
        if self.loaded.lock().unwrap().contains(thread) {
            return Ok(());
        }
        let mut params = self.session_params();
        params["threadId"] = json!(thread);
        self.request("thread/resume", params).await.map_err(|e| format!("{SESSION_GONE}: {e}"))?;
        self.loaded.lock().unwrap().insert(thread.to_owned());
        Ok(())
    }

    /// Delete a session; one Codex no longer has counts as deleted.
    pub async fn delete_thread(&self, thread: &str) -> Result<(), String> {
        match self.request("thread/delete", json!({ "threadId": thread })).await {
            Err(e) if e.contains("no rollout") || e.contains("not found") => Ok(()),
            result => result.map(|_| ()),
        }
    }

    /// Run one turn in `thread`; the answer is the final message, as JSON.
    pub async fn turn(
        &self,
        thread: &str,
        prompt: &str,
        schema: &Value,
        cancel: &Notify,
        mut progress: impl FnMut(String),
    ) -> Result<Outcome<Value>, String> {
        self.ensure_loaded(thread).await?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        {
            let mut subs = self.subscribers.lock().unwrap();
            if subs.contains_key(thread) {
                return Err("this session is busy with another turn".into());
            }
            subs.insert(thread.to_owned(), tx);
        }
        let _subscription = Subscription { codex: self, thread: thread.to_owned() };

        let params = json!({
            "threadId": thread,
            "input": [{ "type": "text", "text": prompt }],
            "outputSchema": schema,
        });
        let started = self.request("turn/start", params).await?;
        let turn_id = started["turn"]["id"].as_str().unwrap_or_default().to_owned();

        let mut state = TurnState::default();
        let deadline = tokio::time::sleep(self.cfg.timeout);
        tokio::pin!(deadline);
        let interrupt = || self.request("turn/interrupt", json!({ "threadId": thread, "turnId": turn_id }));
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else {
                        return Err(self.exited.lock().unwrap().clone().unwrap_or_else(|| "codex app-server stopped".into()));
                    };
                    if let Some(done) = state.handle(&msg, &turn_id, &mut progress) {
                        done?;
                        break;
                    }
                }
                _ = cancel.notified() => {
                    let _ = interrupt().await;
                    return Err("cancelled".into());
                }
                _ = &mut deadline => {
                    let _ = interrupt().await;
                    return Err(format!("timed out after {} minutes", self.cfg.timeout.as_secs() / 60));
                }
            }
        }
        let text = state.last_message.ok_or_else(|| {
            state.last_error.clone().map_or("codex finished the turn without an answer".into(), |e| format!("codex finished without an answer: {e}"))
        })?;
        let answer = serde_json::from_str(text.trim())
            .map_err(|e| format!("codex answer is not JSON ({e}): {}", one_line(&text, 400)))?;
        Ok(Outcome {
            answer,
            commands: state.commands,
            input_tokens: state.input_tokens,
            cached_input_tokens: state.cached_input_tokens,
            output_tokens: state.output_tokens,
        })
    }
}

#[derive(Default)]
struct TurnState {
    last_message: Option<String>,
    last_error: Option<String>,
    commands: u32,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
}

impl TurnState {
    /// `Some(result)` once the turn is over.
    fn handle(&mut self, msg: &Value, turn_id: &str, progress: &mut impl FnMut(String)) -> Option<Result<(), String>> {
        let p = &msg["params"];
        if p["turnId"].as_str().is_some_and(|t| t != turn_id) {
            return None;
        }
        let item = &p["item"];
        match msg["method"].as_str().unwrap_or_default() {
            "item/started" if item["type"] == "commandExecution" => {
                self.commands += 1;
                progress(format!("$ {}", unwrap_shell(item["command"].as_str().unwrap_or_default())));
            }
            "item/completed" => match item["type"].as_str().unwrap_or_default() {
                "agentMessage" => {
                    let text = item["text"].as_str().unwrap_or_default();
                    if !text.trim_start().starts_with('{') {
                        progress(format!("› {}", one_line(text, 300)));
                    }
                    self.last_message = Some(text.to_owned());
                }
                "reasoning" => {
                    let summary: Vec<&str> = item["summary"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                    if !summary.is_empty() {
                        progress(format!("… {}", one_line(&summary.join(" "), 300)));
                    }
                }
                "contextCompaction" => progress("… Codex compacted the session context".into()),
                _ => {}
            },
            // `last` is one model request; summing them gives the turn's usage.
            "thread/tokenUsage/updated" => {
                let last = &p["tokenUsage"]["last"];
                self.input_tokens += last["inputTokens"].as_u64().unwrap_or(0);
                self.cached_input_tokens += last["cachedInputTokens"].as_u64().unwrap_or(0);
                self.output_tokens += last["outputTokens"].as_u64().unwrap_or(0);
            }
            // Transient errors (reconnects) are retried by Codex and only reported.
            "error" => {
                let message = p["error"]["message"].as_str().unwrap_or_default().to_owned();
                progress(format!("! {message}"));
                self.last_error = Some(message);
            }
            "turn/completed" if p["turn"]["id"] == turn_id => {
                let turn = &p["turn"];
                return Some(match turn["status"].as_str().unwrap_or_default() {
                    "completed" => Ok(()),
                    "interrupted" => Err("cancelled".into()),
                    _ => Err(format!(
                        "codex turn failed: {}",
                        turn["error"]["message"].as_str().or(self.last_error.as_deref()).unwrap_or("unknown error")
                    )),
                });
            }
            _ => {}
        }
        None
    }
}

/// `/usr/bin/zsh -lc 'rg -n '"'"'fn main'"'"''` → `rg -n 'fn main'`: decode the
/// single POSIX shell word after `-lc` so progress shows the command as written.
fn unwrap_shell(command: &str) -> String {
    let Some(i) = command.find(" -lc ") else { return command.to_owned() };
    let mut chars = command[i + 5..].trim_start().chars();
    let mut word = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\'' => word.extend(chars.by_ref().take_while(|&c| c != '\'')),
            '"' => {
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(e @ ('$' | '`' | '"' | '\\')) => word.push(e),
                            Some('\n') | None => {}
                            Some(e) => word.extend(['\\', e]),
                        },
                        c => word.push(c),
                    }
                }
            }
            '\\' => word.extend(chars.next()),
            c if c.is_whitespace() => break,
            c => word.push(c),
        }
    }
    word
}
