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
use std::sync::Arc;
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
    /// Identifier used in flags, cache keys and the API.
    pub fn id(self) -> &'static str {
        match self {
            Kind::Codex => "codex",
            Kind::Claude => "claude",
            Kind::Anthropic => "anthropic",
            Kind::OpenAi => "openai",
        }
    }
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

    /// Run one turn in `session` whose final answer must match `schema`.
    pub async fn turn<T: DeserializeOwned>(
        &self,
        session: &str,
        prompt: &str,
        schema: &Value,
        cancel: &Notify,
        progress: impl FnMut(String),
    ) -> Result<Outcome<T>, String> {
        let out = match self {
            Engine::Codex(c) => c.turn(session, prompt, schema, cancel, progress).await?,
            Engine::Claude(c) => c.turn(session, prompt, schema, cancel, progress).await?,
            Engine::Anthropic(c) => c.turn(session, prompt, schema, cancel, progress).await?,
            Engine::OpenAi(c) => c.turn(session, prompt, schema, cancel, progress).await?,
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
