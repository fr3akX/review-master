//! Native Anthropic agent: repository-only tools and persisted Messages history.
//!
//! Unlike `claude -p`, no external agent process runs. Each completed exchange is
//! saved under the selected cache (see `native.rs`), so forks inherit tool results,
//! follow-ups can resume after a restart, and no transcript descriptors stay open.

use crate::anthropic_wire::Transport;
use crate::engine::{one_line, Outcome};
use crate::native::{BusySet, Sessions, ANSWER_DESCRIPTION, SYSTEM};
use crate::repo_tools::Tools;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::sync::Notify;

pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";

/// Move content blocks into the history without serializing/copying a tool response.
fn message(role: &str, blocks: Vec<Value>) -> Value {
    let mut fields = serde_json::Map::with_capacity(2);
    fields.insert("role".into(), Value::String(role.into()));
    fields.insert("content".into(), Value::Array(blocks));
    Value::Object(fields)
}

pub struct Anthropic {
    model: String,
    effort: Option<String>,
    transport: Transport,
    tools: Tools,
    sessions: Sessions,
    timeout: Duration,
    busy: BusySet,
}

impl Anthropic {
    /// `effort` picks the thinking request for the model (see `anthropic_wire::thinking_for`).
    pub async fn start(root: &Path, cache: &Path, model: Option<String>, effort: Option<String>, timeout: Duration) -> Result<Self, String> {
        let model = model.unwrap_or_else(|| DEFAULT_MODEL.into());
        let thinking = effort.as_deref().map(|e| crate::anthropic_wire::thinking_for(&model, e)).transpose()?.flatten();
        let transport = Transport::new(&model, thinking).await?;
        Ok(Self {
            model, effort, transport, tools: Tools::new(root)?, sessions: Sessions::new(root, cache, "anthropic-sessions")?,
            timeout, busy: BusySet::default(),
        })
    }

    pub fn model(&self) -> Option<&str> { Some(&self.model) }

    pub fn effort(&self) -> Option<&str> { self.effort.as_deref() }

    pub fn start_session(&self, name: &str) -> Result<String, String> { self.sessions.create(name, None) }

    pub fn fork(&self, parent: &str, name: &str) -> Result<String, String> { self.sessions.create(name, Some(parent)) }

    pub fn delete_session(&self, id: &str) -> Result<(), String> {
        if self.busy.contains(id) { return Err("this session is busy with another turn".into()); }
        self.sessions.delete(id)
    }

    pub async fn turn(
        &self, id: &str, prompt: &str, schema: &Value, cancel: &Notify, mut progress: impl FnMut(String),
    ) -> Result<Outcome<Value>, String> {
        let _busy = self.busy.claim(id)?;
        let run = self.run(id, prompt, schema, &mut progress);
        tokio::select! {
            result = run => result,
            _ = cancel.notified() => Err("cancelled".into()),
            _ = tokio::time::sleep(self.timeout) => Err(format!("timed out after {} minutes", self.timeout.as_secs() / 60)),
        }
    }

    async fn run(
        &self, id: &str, prompt: &str, schema: &Value, progress: &mut impl FnMut(String),
    ) -> Result<Outcome<Value>, String> {
        let validator = jsonschema::validator_for(schema).map_err(|e| format!("invalid answer schema: {e}"))?;
        let mut session = self.sessions.load(id)?;
        session.messages.push(message("user", vec![json!({ "type": "text", "text": prompt })]));
        let mut tools = Tools::definitions();
        tools.push(json!({ "name": "_answer", "description": ANSWER_DESCRIPTION, "input_schema": schema }));
        let mut outcome = Outcome { answer: Value::Null, commands: 0, input_tokens: 0, cached_input_tokens: 0, output_tokens: 0 };
        loop {
            let reply = self.transport.message(id, SYSTEM, &session.messages, &tools, progress).await?;
            outcome.input_tokens += reply.input_tokens;
            outcome.cached_input_tokens += reply.cached_input_tokens;
            outcome.output_tokens += reply.output_tokens;
            if reply.stop_reason == "max_tokens" {
                return Err("anthropic response exceeded its output limit before completing the answer".into());
            }
            let calls: Vec<&Value> = reply.content.iter().filter(|b| b["type"] == "tool_use").collect();
            if calls.is_empty() {
                return Err(format!("anthropic finished without calling _answer (stop reason: {})", reply.stop_reason));
            }
            let mut results = Vec::with_capacity(calls.len());
            let mut answer = None;
            for call in &calls {
                let call_id = call["id"].as_str().ok_or("anthropic returned a tool call without an id")?;
                let name = call["name"].as_str().unwrap_or("");
                let input = &call["input"];
                let result = if name == "_answer" {
                    if calls.len() != 1 {
                        Err("Finish reading the other tool results before submitting _answer alone.".into())
                    } else if let Err(error) = validator.validate(input) {
                        Err(format!("Answer does not match the required schema: {}", one_line(&error.to_string(), 500)))
                    } else {
                        answer = Some(input.clone());
                        Ok("Answer accepted.".into())
                    }
                } else {
                    outcome.commands += 1;
                    progress(format!("$ {name} {}", one_line(&input.to_string(), 240)));
                    self.tools.call(name, input).await
                };
                let (text, is_error) = match result { Ok(text) => (text, false), Err(e) => (e, true) };
                results.push(json!({ "type": "tool_result", "tool_use_id": call_id, "content": text, "is_error": is_error }));
            }
            session.messages.push(message("assistant", reply.content));
            session.messages.push(message("user", results));
            // Never persist a dangling tool_use: resuming after cancellation starts
            // from the last complete exchange, not an invalid API conversation.
            self.sessions.save(id, &session)?;
            if let Some(answer) = answer {
                outcome.answer = answer;
                return Ok(outcome);
            }
        }
    }
}
