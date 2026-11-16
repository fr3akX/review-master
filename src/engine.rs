//! Engines: Codex app-server, Claude Code CLI, native Anthropic Messages, or any
//! OpenAI-compatible Chat Completions server.
//!
//! All keep persisted, read-only sessions. review-master starts one session per
//! reviewed pull request; the re-anchoring follow-up continues it.

use crate::anthropic::Anthropic;
use crate::claude::Claude;
use crate::codex::Codex;
use crate::openai::OpenAi;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Start of the error for a turn in a session that no longer exists.
pub const SESSION_GONE: &str = "session not found";

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Codex,
    Claude,
    Anthropic,
    #[value(name = "openai")]
    OpenAi,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Codex, Kind::Claude, Kind::Anthropic, Kind::OpenAi];

    /// Identifier used in flags, cache keys and the API.
    pub fn id(self) -> &'static str {
        match self {
            Kind::Codex => "codex",
            Kind::Claude => "claude",
            Kind::Anthropic => "anthropic",
            Kind::OpenAi => "openai",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Kind::Codex => "Codex CLI (codex app-server)",
            Kind::Claude => "Claude Code CLI (claude -p)",
            Kind::Anthropic => "built-in agent: Claude subscription or Anthropic API key",
            Kind::OpenAi => "built-in agent: OpenAI-compatible server (OpenAI, OpenRouter, llama.cpp, vLLM, Ollama, …)",
        }
    }

    /// Levels to offer; the engine itself has the last word (Codex models and
    /// OpenAI-compatible servers differ), so others can still be given.
    pub fn effort_levels(self) -> &'static [&'static str] {
        match self {
            Kind::Codex => &["minimal", "low", "medium", "high", "xhigh"],
            Kind::Claude | Kind::Anthropic => &["low", "medium", "high", "xhigh", "max"],
            Kind::OpenAi => &["none", "minimal", "low", "medium", "high"],
        }
    }
}

/// A model an engine offers.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ModelInfo {
    pub id: String,
    /// Display name, when it differs from the id.
    pub label: Option<String>,
    /// The engine's default.
    pub default: bool,
    /// Reasoning efforts this model takes, when the engine says (Codex).
    pub efforts: Vec<String>,
}

impl ModelInfo {
    fn plain(id: &str, label: Option<&str>, default: bool) -> ModelInfo {
        ModelInfo { id: id.to_owned(), label: label.map(str::to_owned), default, efforts: Vec::new() }
    }
}

/// How engines are reached: CLI executables and the OpenAI-compatible server.
#[derive(Clone, Debug)]
pub struct EngineOpts {
    pub codex: String,
    pub claude: String,
    pub openai_base_url: String,
    pub openai_api_key: Option<String>,
    /// Limit for one turn.
    pub timeout: Duration,
}

/// Start an engine over the checkout at `root`.
pub async fn start(kind: Kind, model: Option<String>, o: &EngineOpts, root: &Path, cache_dir: &Path) -> Result<Engine, String> {
    let timeout = o.timeout;
    Ok(match kind {
        Kind::Codex => {
            let cfg = crate::codex::CodexConfig { bin: o.codex.clone(), model, timeout };
            Engine::Codex(Codex::start(cfg, root).await?)
        }
        Kind::Claude => {
            let cfg = crate::claude::ClaudeConfig { bin: o.claude.clone(), model, timeout };
            Engine::Claude(Arc::new(Claude::start(cfg, root).await?))
        }
        Kind::Anthropic => {
            let model = model.or_else(|| Some(crate::anthropic::DEFAULT_MODEL.to_owned()));
            Engine::Anthropic(Arc::new(Anthropic::start(root, cache_dir, model, timeout).await?))
        }
        Kind::OpenAi => {
            let cfg = crate::openai::OpenAiConfig { base_url: o.openai_base_url.clone(), api_key: o.openai_api_key.clone(), model, timeout };
            Engine::OpenAi(Arc::new(OpenAi::start(root, cache_dir, cfg).await?))
        }
    })
}

/// The models `kind` offers. Codex asks the running engine when it is Codex, else a
/// short-lived app-server; Claude Code takes its aliases; the OpenAI-compatible server
/// lists its own. Any other name can still be typed.
pub async fn list_models(kind: Kind, current: &Engine, o: &EngineOpts, root: &Path) -> Result<Vec<ModelInfo>, String> {
    match kind {
        Kind::Codex => match current {
            Engine::Codex(c) => c.list_models().await,
            _ => {
                let cfg = crate::codex::CodexConfig { bin: o.codex.clone(), model: None, timeout: o.timeout };
                Codex::start(cfg, root).await?.list_models().await
            }
        },
        Kind::Claude => Ok(vec![
            ModelInfo::plain("sonnet", Some("latest Sonnet"), false),
            ModelInfo::plain("opus", Some("latest Opus"), false),
            ModelInfo::plain("haiku", Some("latest Haiku"), false),
        ]),
        Kind::Anthropic => Ok(vec![ModelInfo::plain(crate::anthropic::DEFAULT_MODEL, None, true)]),
        Kind::OpenAi => {
            let ids = crate::openai::list_models(&o.openai_base_url, o.openai_api_key.as_deref()).await?;
            let only = ids.len() == 1;
            Ok(ids.iter().map(|id| ModelInfo::plain(id, None, only)).collect())
        }
    }
}

/// Whether an engine looks usable right now, and why (shown next to it).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Availability {
    pub ready: bool,
    pub note: String,
    /// Whether review-master can run the engine's sign-in for you.
    pub can_login: bool,
}

fn on_path(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

fn env_set(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.trim().is_empty())
}

fn quiet(bin: &str, args: &[&str]) -> Option<std::process::Output> {
    Command::new(bin).args(args).stdin(Stdio::null()).output().ok()
}

/// What a status command said (`codex login status` writes to stderr).
fn said(out: &std::process::Output) -> String {
    let text = if out.stdout.iter().any(|b| !b.is_ascii_whitespace()) { &out.stdout } else { &out.stderr };
    one_line(String::from_utf8_lossy(text).trim(), 80)
}

/// Checks each engine's prerequisites: the CLI and its login, or credentials, or the server.
pub fn availability(kind: Kind, o: &EngineOpts) -> Availability {
    let (ready, note, can_login) = match kind {
        Kind::Codex if !on_path(&o.codex) => (false, format!("`{}` not found on PATH", o.codex), false),
        Kind::Codex => match quiet(&o.codex, &["login", "status"]) {
            Some(out) if out.status.success() => (true, Some(said(&out)).filter(|s| !s.is_empty()).unwrap_or_else(|| "signed in".to_owned()), true),
            _ => (false, "not signed in".to_owned(), true),
        },
        Kind::Claude if !on_path(&o.claude) => (false, format!("`{}` not found on PATH", o.claude), false),
        Kind::Claude => {
            let status = quiet(&o.claude, &["auth", "status"]).and_then(|out| serde_json::from_slice::<Value>(&out.stdout).ok());
            match status {
                Some(s) if s["loggedIn"].as_bool() == Some(true) => {
                    (true, format!("signed in{}", s["authMethod"].as_str().map(|m| format!(" ({m})")).unwrap_or_default()), true)
                }
                Some(_) => (false, "not signed in".to_owned(), true),
                None => (true, "found".to_owned(), false),
            }
        }
        Kind::Anthropic if env_set("ANTHROPIC_API_KEY") || env_set("ANTHROPIC_OAUTH_TOKEN") => (true, "credentials in the environment".to_owned(), false),
        Kind::Anthropic if crate::anthropic_auth::credentials_path().is_ok_and(|p| p.is_file()) => (true, "signed in".to_owned(), true),
        Kind::Anthropic => (false, "not signed in".to_owned(), true),
        Kind::OpenAi => (true, format!("server {}", o.openai_base_url), false),
    };
    Availability { ready, note, can_login }
}

pub struct Outcome<T> {
    pub answer: T,
    /// Shell commands (Codex) or tool calls (the other engines) the turn ran.
    pub commands: u32,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

pub enum Engine {
    Codex(Arc<Codex>),
    Claude(Arc<Claude>),
    Anthropic(Arc<Anthropic>),
    OpenAi(Arc<OpenAi>),
}

impl Engine {
    pub fn kind(&self) -> Kind {
        match self {
            Engine::Codex(_) => Kind::Codex,
            Engine::Claude(_) => Kind::Claude,
            Engine::Anthropic(_) => Kind::Anthropic,
            Engine::OpenAi(_) => Kind::OpenAi,
        }
    }

    pub fn model(&self) -> Option<&str> {
        match self {
            Engine::Codex(c) => c.model(),
            Engine::Claude(c) => c.model(),
            Engine::Anthropic(c) => c.model(),
            Engine::OpenAi(c) => c.model(),
        }
    }

    /// Levels to offer for this engine (see [`Kind::effort_levels`]).
    pub fn effort_levels(&self) -> &'static [&'static str] {
        self.kind().effort_levels()
    }

    /// Reject an effort the engine can be told up front it will not take: blank or
    /// multi-word, or (native Anthropic) not a level of this model.
    pub fn check_effort(&self, effort: &str) -> Result<(), String> {
        if effort.is_empty() || effort.len() > 32 || !effort.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(format!("`{effort}` is not an effort level"));
        }
        match self {
            Engine::Anthropic(c) => c.check_effort(effort),
            _ => Ok(()),
        }
    }

    /// A new persisted session.
    pub async fn start_session(&self, name: &str) -> Result<String, String> {
        match self {
            Engine::Codex(c) => c.start_thread(name).await,
            Engine::Claude(c) => c.start_session(name),
            Engine::Anthropic(c) => c.start_session(name),
            Engine::OpenAi(c) => c.start_session(name),
        }
    }

    /// A new persisted session that starts with everything `parent` has seen.
    /// Unused today; kept for cxplr parity — forking is the seam for parallel
    /// reviewers and re-review follow-ups.
    #[allow(dead_code)]
    pub async fn fork(&self, parent: &str, name: &str) -> Result<String, String> {
        match self {
            Engine::Codex(c) => c.fork(parent, name).await,
            Engine::Claude(c) => c.fork(parent, name),
            Engine::Anthropic(c) => c.fork(parent, name),
            Engine::OpenAi(c) => c.fork(parent, name),
        }
    }

    /// Delete a session; one that no longer exists counts as deleted. Unused
    /// today; kept for cxplr parity (session maintenance).
    #[allow(dead_code)]
    pub async fn delete_session(&self, id: &str) -> Result<(), String> {
        match self {
            Engine::Codex(c) => c.delete_thread(id).await,
            Engine::Claude(c) => c.delete_session(id),
            Engine::Anthropic(c) => c.delete_session(id),
            Engine::OpenAi(c) => c.delete_session(id),
        }
    }

    /// Run one turn in `session` whose final answer must match `schema`; `effort`
    /// overrides the engine's default reasoning effort for this turn.
    pub async fn turn<T: DeserializeOwned>(
        &self,
        session: &str,
        prompt: &str,
        schema: &Value,
        effort: Option<&str>,
        cancel: &Notify,
        progress: impl FnMut(String),
    ) -> Result<Outcome<T>, String> {
        let out = match self {
            Engine::Codex(c) => c.turn(session, prompt, schema, effort, cancel, progress).await?,
            Engine::Claude(c) => c.turn(session, prompt, schema, effort, cancel, progress).await?,
            Engine::Anthropic(c) => c.turn(session, prompt, schema, effort, cancel, progress).await?,
            Engine::OpenAi(c) => c.turn(session, prompt, schema, effort, cancel, progress).await?,
        };
        let answer = T::deserialize(&out.answer).map_err(|e| {
            format!("{} answer does not match the schema ({e}): {}", self.kind().id(), one_line(&out.answer.to_string(), 400))
        })?;
        Ok(Outcome {
            answer,
            commands: out.commands,
            input_tokens: out.input_tokens,
            cached_input_tokens: out.cached_input_tokens,
            output_tokens: out.output_tokens,
        })
    }
}

/// `text` on one line, cut to `max` characters.
pub fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}
