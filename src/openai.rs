//! OpenAI-compatible engine: review-master's own read-only agent loop over Chat Completions
//! with function calling, against any server that implements it (OpenAI, OpenRouter,
//! Groq, vLLM, llama.cpp, Ollama, LM Studio, …). Histories are stored like native
//! Anthropic's (see `native.rs`), in OpenAI message format.

use crate::engine::{one_line, Outcome};
use crate::native::{BusySet, Sessions, ANSWER_DESCRIPTION, SYSTEM};
use crate::repo_tools::Tools;
use serde::ser::{SerializeSeq, Serializer};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::sync::Notify;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Replies without a tool call tolerated in one turn before giving up.
const MAX_NUDGES: u32 = 2;
const NUDGE: &str = "Call the _answer tool now with the final JSON object, or keep investigating with _list, _glob, _grep and _read.";

pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    /// `None`: use the server's only model.
    pub model: Option<String>,
    pub timeout: Duration,
}

pub struct OpenAi {
    client: reqwest::Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    tools: Tools,
    sessions: Sessions,
    timeout: Duration,
    busy: BusySet,
}

/// The API root: `http://host:8080` → `http://host:8080/v1`; a URL with a path is kept.
fn api_base(raw: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|e| format!("invalid OpenAI base URL {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("invalid OpenAI base URL {raw:?}: use http:// or https://"));
    }
    let base = format!("{}://{}{}", url.scheme(), url.host_str().unwrap_or_default(), url.port().map(|p| format!(":{p}")).unwrap_or_default());
    let path = url.path().trim_end_matches('/');
    Ok(if path.is_empty() { format!("{base}/v1") } else { format!("{base}{path}") })
}

/// The final answer from a reply that wrote it as text (optionally fenced) instead of
/// calling `_answer`.
fn json_answer(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str::<Value>(text.get(start..=end)?).ok().filter(Value::is_object)
}

/// The repository tools and `_answer` as Chat Completions function tools.
fn function_tools(schema: &Value) -> Vec<Value> {
    let answer = json!({ "name": "_answer", "description": ANSWER_DESCRIPTION, "input_schema": schema });
    Tools::definitions()
        .into_iter()
        .chain(std::iter::once(answer))
        .map(|t| json!({ "type": "function", "function": { "name": t["name"], "description": t["description"], "parameters": t["input_schema"] } }))
        .collect()
}

/// Tool calls with an id and string arguments, as they go back into the history,
/// paired with their parsed arguments.
fn normalize_calls(calls: &[Value]) -> Vec<(Value, Result<Value, String>)> {
    calls
        .iter()
        .map(|call| {
            let id = call["id"].as_str().filter(|s| !s.is_empty()).map_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()), str::to_owned);
            let name = call["function"]["name"].as_str().unwrap_or_default().to_owned();
            let (raw, args) = match &call["function"]["arguments"] {
                Value::String(s) if s.trim().is_empty() => ("{}".to_owned(), Ok(json!({}))),
                Value::String(s) => (s.clone(), serde_json::from_str(s).map_err(|e| format!("arguments are not valid JSON: {e}"))),
                Value::Null => ("{}".to_owned(), Ok(json!({}))),
                other => (other.to_string(), Ok(other.clone())),
            };
            let stored = json!({ "id": id, "type": "function", "function": { "name": name, "arguments": raw } });
            (stored, args)
        })
        .collect()
}

/// The system prompt followed by the history, serialized without copying it.
struct Messages<'a> {
    system: &'a Value,
    rest: &'a [Value],
}

impl Serialize for Messages<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.rest.len() + 1))?;
        seq.serialize_element(self.system)?;
        for m in self.rest {
            seq.serialize_element(m)?;
        }
        seq.end()
    }
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: Messages<'a>,
    tools: &'a [Value],
    tool_choice: &'static str,
}

impl OpenAi {
    pub async fn start(root: &Path, cache: &Path, cfg: OpenAiConfig) -> Result<Self, String> {
        let base = api_base(&cfg.base_url)?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| format!("HTTP client: {e}"))?;
        let model = match cfg.model {
            Some(model) => model,
            None => only_model(&client, &base, cfg.api_key.as_deref()).await?,
        };
        Ok(Self {
            client,
            endpoint: format!("{base}/chat/completions"),
            api_key: cfg.api_key,
            model,
            tools: Tools::new(root)?,
            sessions: Sessions::new(root, cache, "openai-sessions")?,
            timeout: cfg.timeout,
            busy: BusySet::default(),
        })
    }

    pub fn model(&self) -> Option<&str> {
        Some(&self.model)
    }

    pub fn start_session(&self, name: &str) -> Result<String, String> {
        self.sessions.create(name, None)
    }

    pub fn fork(&self, parent: &str, name: &str) -> Result<String, String> {
        self.sessions.create(name, Some(parent))
    }

    pub fn delete_session(&self, id: &str) -> Result<(), String> {
        if self.busy.contains(id) {
            return Err("this session is busy with another turn".into());
        }
        self.sessions.delete(id)
    }

    pub async fn turn(
        &self,
        id: &str,
        prompt: &str,
        schema: &Value,
        cancel: &Notify,
        mut progress: impl FnMut(String),
    ) -> Result<Outcome<Value>, String> {
        let _busy = self.busy.claim(id)?;
        let run = self.run(id, prompt, schema, &mut progress);
        tokio::select! {
            result = run => result,
            _ = cancel.notified() => Err("cancelled".into()),
            _ = tokio::time::sleep(self.timeout) => Err(format!("timed out after {} minutes", self.timeout.as_secs() / 60)),
        }
    }

    async fn complete(&self, messages: &[Value], tools: &[Value]) -> Result<Value, String> {
        let system = json!({ "role": "system", "content": SYSTEM });
        let body = Request { model: &self.model, messages: Messages { system: &system, rest: messages }, tools, tool_choice: "auto" };
        let mut req = self.client.post(&self.endpoint).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| format!("cannot reach {}: {e}", self.endpoint))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("reading the response from {}: {e}", self.endpoint))?;
        if !status.is_success() {
            let parsed = serde_json::from_str::<Value>(&text).ok();
            let why = parsed
                .as_ref()
                .and_then(|v| v["error"]["message"].as_str().or(v["error"].as_str()).or(v["message"].as_str()))
                .unwrap_or(&text);
            return Err(format!("{} returned {status}: {}", self.endpoint, one_line(why, 600)));
        }
        serde_json::from_str(&text).map_err(|e| format!("{} did not return JSON ({e}): {}", self.endpoint, one_line(&text, 300)))
    }

    async fn run(&self, id: &str, prompt: &str, schema: &Value, progress: &mut impl FnMut(String)) -> Result<Outcome<Value>, String> {
        let validator = jsonschema::validator_for(schema).map_err(|e| format!("invalid answer schema: {e}"))?;
        let mut session = self.sessions.load(id)?;
        session.messages.push(json!({ "role": "user", "content": prompt }));
        let tools = function_tools(schema);
        let mut outcome = Outcome { answer: Value::Null, commands: 0, input_tokens: 0, cached_input_tokens: 0, output_tokens: 0 };
        let mut nudges = 0;
        loop {
            let reply = self.complete(&session.messages, &tools).await?;
            let usage = &reply["usage"];
            outcome.input_tokens += usage["prompt_tokens"].as_u64().unwrap_or(0);
            outcome.cached_input_tokens += usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
            outcome.output_tokens += usage["completion_tokens"].as_u64().unwrap_or(0);
            let choice = &reply["choices"][0];
            let message = &choice["message"];
            if !message.is_object() {
                return Err(format!("the reply has no message: {}", one_line(&reply.to_string(), 300)));
            }
            let finish = choice["finish_reason"].as_str().unwrap_or_default();
            if let Some(thinking) = message["reasoning_content"].as_str().or(message["reasoning"].as_str()) {
                if !thinking.trim().is_empty() {
                    progress(format!("… {}", one_line(thinking, 300)));
                }
            }
            let content = message["content"].as_str().unwrap_or_default();
            let calls = message["tool_calls"].as_array().map_or(&[][..], Vec::as_slice);
            if calls.is_empty() {
                if finish == "length" {
                    return Err("the model hit the server's output limit before answering".into());
                }
                // Some models write the answer as text instead of calling _answer.
                if let Some(answer) = json_answer(content).filter(|a| validator.is_valid(a)) {
                    session.messages.push(json!({ "role": "assistant", "content": content }));
                    self.sessions.save(id, &session)?;
                    outcome.answer = answer;
                    return Ok(outcome);
                }
                if nudges == MAX_NUDGES {
                    return Err(format!("the model stopped without calling _answer: {}", one_line(content, 300)));
                }
                nudges += 1;
                progress(format!("› {}", one_line(content, 300)));
                session.messages.push(json!({ "role": "assistant", "content": content }));
                session.messages.push(json!({ "role": "user", "content": NUDGE }));
                continue;
            }
            if !content.trim().is_empty() {
                progress(format!("› {}", one_line(content, 300)));
            }
            let calls = normalize_calls(calls);
            let mut results = Vec::with_capacity(calls.len());
            let mut answer = None;
            for (call, args) in &calls {
                let name = call["function"]["name"].as_str().unwrap_or_default();
                let result = match args {
                    Err(e) => Err(e.clone()),
                    Ok(input) if name == "_answer" => {
                        if calls.len() != 1 {
                            Err("Finish reading the other tool results before submitting _answer alone.".into())
                        } else if let Err(error) = validator.validate(input) {
                            Err(format!("Answer does not match the required schema: {}", one_line(&error.to_string(), 500)))
                        } else {
                            answer = Some(input.clone());
                            Ok("Answer accepted.".into())
                        }
                    }
                    Ok(input) => {
                        outcome.commands += 1;
                        progress(format!("$ {name} {}", one_line(&input.to_string(), 240)));
                        self.tools.call(name, input).await
                    }
                };
                let text = result.unwrap_or_else(|e| format!("Error: {e}"));
                results.push(json!({ "role": "tool", "tool_call_id": call["id"], "content": text }));
            }
            let stored: Vec<Value> = calls.into_iter().map(|(call, _)| call).collect();
            let content = if content.is_empty() { Value::Null } else { Value::String(content.to_owned()) };
            session.messages.push(json!({ "role": "assistant", "content": content, "tool_calls": stored }));
            session.messages.extend(results);
            // Only complete exchanges are saved: no tool call without its result.
            self.sessions.save(id, &session)?;
            if let Some(answer) = answer {
                outcome.answer = answer;
                return Ok(outcome);
            }
        }
    }
}

/// The model ids `<base>/models` lists.
async fn models(client: &reqwest::Client, base: &str, key: Option<&str>) -> Result<Vec<String>, String> {
    let mut req = client.get(format!("{base}/models"));
    if let Some(key) = key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await.map_err(|e| format!("cannot reach {base}: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("{base}/models did not return JSON: {e}"))?;
    if !status.is_success() {
        return Err(format!("{base}/models returned {status}"));
    }
    Ok(body["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect())
}

/// The models a server offers, for `--configure`; `base_url` as the user wrote it.
pub async fn list_models(base_url: &str, key: Option<&str>) -> Result<Vec<String>, String> {
    let base = api_base(base_url)?;
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
    models(&client, &base, key).await
}

/// The model of a server that serves exactly one (typical for local servers).
async fn only_model(client: &reqwest::Client, base: &str, key: Option<&str>) -> Result<String, String> {
    let ids = models(client, base, key).await.map_err(|e| format!("{e}; pass --model"))?;
    match ids.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("{base} lists no models; pass --model")),
        many => Err(format!("{base} serves {} models; choose one with --model: {}", many.len(), many.iter().take(12).cloned().collect::<Vec<_>>().join(", "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_get_v1_only_without_a_path() {
        assert_eq!(api_base("http://llm.example:8080/").unwrap(), "http://llm.example:8080/v1");
        assert_eq!(api_base("http://localhost:11434").unwrap(), "http://localhost:11434/v1");
        assert_eq!(api_base("https://api.openai.com/v1").unwrap(), "https://api.openai.com/v1");
        assert_eq!(api_base("https://openrouter.ai/api/v1/").unwrap(), "https://openrouter.ai/api/v1");
        assert!(api_base("ftp://example.com").is_err());
        assert!(api_base("not a url").is_err());
    }

    #[test]
    fn text_answers_are_found_with_or_without_fences() {
        assert_eq!(json_answer("```json\n{\"a\": 1}\n```"), Some(json!({ "a": 1 })));
        assert_eq!(json_answer("Here it is: {\"a\": {\"b\": 2}} done"), Some(json!({ "a": { "b": 2 } })));
        assert_eq!(json_answer("no json here"), None);
        assert_eq!(json_answer("{broken"), None);
    }

    #[test]
    fn tool_calls_get_ids_and_bad_arguments_become_errors() {
        let calls = [
            json!({ "id": "c1", "function": { "name": "_read", "arguments": "{\"path\":\"a.py\"}" } }),
            json!({ "function": { "name": "_list", "arguments": "" } }),
            json!({ "id": "c3", "function": { "name": "_grep", "arguments": "{oops" } }),
            json!({ "id": "c4", "function": { "name": "_glob", "arguments": { "pattern": "*.rs" } } }),
        ];
        let out = normalize_calls(&calls);
        assert_eq!(out[0].0["id"], "c1");
        assert_eq!(out[0].1.as_ref().unwrap(), &json!({ "path": "a.py" }));
        assert!(out[1].0["id"].as_str().unwrap().starts_with("call_"));
        assert_eq!(out[1].1.as_ref().unwrap(), &json!({}));
        assert!(out[2].1.as_ref().unwrap_err().starts_with("arguments are not valid JSON"));
        // History keeps arguments as a JSON string, as the API expects.
        assert_eq!(out[3].0["function"]["arguments"], "{\"pattern\":\"*.rs\"}");
        assert_eq!(out[3].1.as_ref().unwrap(), &json!({ "pattern": "*.rs" }));
    }
}
