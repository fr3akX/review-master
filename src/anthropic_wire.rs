//! Native Anthropic Messages transport: one streamed `/v1/messages` call per model step.
//!
//! OAuth (Claude Pro/Max) requests reproduce Claude Code's wire identity the way
//! oh-my-pi does: `claude-cli` User-Agent, Stainless/`x-app` headers, the Claude Code
//! beta profile, the identity system block, the `x-anthropic-billing-header` block
//! with its `cc_version` fingerprint and xxHash64 `cch` body attestation, and the
//! `{device_id, session_id, account_uuid}` metadata user id. API-key requests carry
//! none of that identity. Prompt caching anchors the last tool, the stable system
//! block and the two newest messages (at most 4 breakpoints).
//!
//! Constants and algorithms adapted from oh-my-pi (MIT License, Copyright (c) Mario
//! Zechner and oh-my-pi contributors): `packages/ai/src/providers/anthropic.ts`
//! (billing header, cch patch, caching), `anthropic-headers.ts` (headers, betas),
//! `claude-code-fingerprint.ts` (versions), `anthropic-identity.ts` (metadata).

use crate::anthropic_auth::{one_line_bounded, read_bounded, redact, Auth, Secret};
use crate::engine::one_line;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// Pinned Claude Code CLI version (omp's `DEFAULT_CLAUDE_CODE_VERSION`).
pub const DEFAULT_CLAUDE_CODE_VERSION: &str = "2.1.280";
/// `@anthropic-ai/sdk` version bundled by that Claude Code release.
pub const CLAUDE_CODE_SDK_VERSION: &str = "0.112.1";
const VERSION_ENV: &str = "REVIEW_MASTER_CLAUDE_CODE_VERSION";
static ADOPTED_VERSION: Mutex<Option<String>> = Mutex::new(None);

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const ACCEPT_ENCODING: &str = "gzip, deflate, br, zstd";
const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";
const BILLING_SALT: &str = "59cf53e54c78";
const CCH_PLACEHOLDER: &str = "cch=00000";
const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;
const MAX_BREAKPOINTS: usize = 4;
/// Tools whose input must arrive whole (omp buffers its final-answer tool).
const BUFFERED_INPUT_TOOLS: &[&str] = &["_answer"];
const AGENT_BETAS: &[&str] = &[
    "claude-code-20250219",
    "oauth-2025-04-20",
    "interleaved-thinking-2025-05-14",
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "mid-conversation-system-2026-04-07",
    "fallback-credit-2026-06-01",
];
const UTILITY_BETAS: &[&str] = &[
    "oauth-2025-04-20",
    "interleaved-thinking-2025-05-14",
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "structured-outputs-2025-12-15",
];
const MAX_PENDING_SSE: usize = 32 << 20;
const MAX_BLOCKS: usize = 4096;
const ERROR_BODY_LIMIT: usize = 16 * 1024;

/// Claude Code version on the wire: `REVIEW_MASTER_CLAUDE_CODE_VERSION`, else a version
/// adopted from a `claude_code_version_too_old` rejection, else the pinned default.
pub fn claude_code_version() -> String {
    env_version()
        .or_else(|| ADOPTED_VERSION.lock().unwrap().clone())
        .unwrap_or_else(|| DEFAULT_CLAUDE_CODE_VERSION.to_owned())
}

fn env_version() -> Option<String> {
    std::env::var(VERSION_ENV).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

fn semver(v: &str) -> Option<[u64; 3]> {
    let mut parts = v.split('.').map(|p| p.parse::<u64>().ok());
    let out = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(out)
}

/// Minimum version named by a `claude_code_version_too_old` rejection, when newer.
fn required_version(error_text: &str, current: &str) -> Option<String> {
    static REQUIRED: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)version (\d+\.\d+\.\d+) or newer is required").unwrap());
    if !error_text.contains("claude_code_version_too_old") {
        return None;
    }
    let required = REQUIRED.captures(error_text)?.get(1)?.as_str();
    (semver(required)? > semver(current)?).then(|| required.to_owned())
}

/// Adopts a newer required version for this process (never over an explicit pin).
/// True only when the wire version increased, so a retry cannot loop.
fn adopt_required_version(error_text: &str) -> bool {
    if env_version().is_some() {
        return false;
    }
    match required_version(error_text, &claude_code_version()) {
        Some(required) => {
            *ADOPTED_VERSION.lock().unwrap() = Some(required);
            true
        }
        None => false,
    }
}

/// One completed model response.
#[derive(Debug)]
pub struct Reply {
    /// Native content blocks (text, thinking with signature, tool_use with parsed input).
    pub content: Vec<Value>,
    pub stop_reason: String,
    /// All prompt tokens: uncached + cache reads + cache writes.
    pub input_tokens: u64,
    /// Cache reads only.
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

pub struct Transport {
    client: reqwest::Client,
    model: String,
    max_tokens: u32,
    auth: Auth,
    device_id: String,
}

impl Transport {
    /// Selects credentials and builds the HTTP client; sends no request.
    pub async fn new(model: &str) -> Result<Transport, String> {
        let auth = Auth::resolve()?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Transport {
            device_id: device_id(auth.install_id(), auth.account_id()),
            client,
            model: model.to_owned(),
            max_tokens: max_output_tokens(model),
            auth,
        })
    }

    /// Streams one Messages call. Dropping the future drops the connection.
    /// Retries at most once for a newer required Claude Code version (OAuth) and
    /// once after refreshing a rejected stored OAuth token; nothing else.
    pub async fn message(
        &self, session: &str, system: &str, messages: &[Value], tools: &[Value], progress: &mut impl FnMut(String),
    ) -> Result<Reply, String> {
        let oauth = self.auth.is_oauth();
        let (mut version_retry, mut auth_retry) = (oauth, true);
        loop {
            let credential = self.auth.bearer(&self.client).await?;
            let version = claude_code_version();
            let wire = oauth.then(|| OAuthWire {
                version: &version, device_id: &self.device_id, account_id: self.auth.account_id(), session,
            });
            let body = build_body(&self.model, self.max_tokens, system, messages, tools, wire.as_ref());
            let (url, headers) = if oauth {
                (format!("{MESSAGES_URL}?beta=true"), oauth_headers(&credential, session, &version, !tools.is_empty())?)
            } else {
                (MESSAGES_URL.to_owned(), api_key_headers(&credential)?)
            };
            let response = self
                .client
                .post(url)
                .headers(headers)
                .body(body)
                .send()
                .await
                .map_err(|e| format!("anthropic request failed: {}", redact(&e.without_url().to_string())))?;
            let status = response.status();
            if status.is_success() {
                return read_stream(response, progress).await;
            }
            let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
            let (request_id, retry_after) = (header("request-id"), header("retry-after"));
            let text = read_bounded(response, ERROR_BODY_LIMIT).await;
            if version_retry && status.is_client_error() && adopt_required_version(&text) {
                version_retry = false;
                continue;
            }
            if status == reqwest::StatusCode::UNAUTHORIZED && auth_retry {
                auth_retry = false;
                if self.auth.refresh_after_rejection(&self.client, &credential).await? {
                    continue;
                }
            }
            return Err(http_error(status.as_u16(), &text, request_id.as_deref(), retry_after.as_deref(), self.auth.source()));
        }
    }
}

/// Claude 5-generation models get omp's 128k output ceiling; others a 32k request.
fn max_output_tokens(model: &str) -> u32 {
    let generation = model.split('-').filter_map(|p| p.parse::<u32>().ok()).find(|n| *n < 100).unwrap_or(0);
    if generation >= 5 { 128_000 } else { 32_768 }
}

/// Stable per-installation (and per-account) device id, omp's derivation with a
/// review-master hash domain.
fn device_id(install_id: &str, account_id: Option<&str>) -> String {
    let digest = match account_id.filter(|a| !a.is_empty()) {
        Some(account) => Sha256::digest(format!("review-master-claude-device-id-v2\0{install_id}\0{account}")),
        None => Sha256::digest(format!("review-master-claude-device-id-v1:{install_id}")),
    };
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn header_value(value: &str) -> Result<HeaderValue, String> {
    HeaderValue::from_str(value).map_err(|_| "credential or header contains characters invalid in HTTP headers".to_owned())
}

fn stainless_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x64".into(),
        "aarch64" => "arm64".into(),
        "x86" => "x86".into(),
        other => format!("other::{other}"),
    }
}

fn stainless_os() -> String {
    match std::env::consts::OS {
        "macos" => "MacOS".into(),
        "windows" => "Windows".into(),
        "linux" => "Linux".into(),
        "freebsd" => "FreeBSD".into(),
        other => format!("Other::{other}"),
    }
}

/// Claude Code's OAuth header set, in omp's order.
fn oauth_headers(token: &Secret, session: &str, version: &str, agent: bool) -> Result<HeaderMap, String> {
    let betas = if agent { AGENT_BETAS } else { UTILITY_BETAS };
    let mut h = HeaderMap::new();
    let mut put = |name: &'static str, value: HeaderValue| {
        h.insert(HeaderName::from_static(name), value);
    };
    put("accept", HeaderValue::from_static("application/json"));
    put("content-type", HeaderValue::from_static("application/json"));
    put("user-agent", header_value(&format!("claude-cli/{version} (external, cli)"))?);
    put("x-claude-code-session-id", header_value(session)?);
    put("x-stainless-arch", header_value(&stainless_arch())?);
    put("x-stainless-lang", HeaderValue::from_static("js"));
    put("x-stainless-os", header_value(&stainless_os())?);
    put("x-stainless-package-version", HeaderValue::from_static(CLAUDE_CODE_SDK_VERSION));
    put("x-stainless-retry-count", HeaderValue::from_static("0"));
    put("x-stainless-runtime", HeaderValue::from_static("node"));
    put("x-stainless-runtime-version", HeaderValue::from_static("v26.3.0"));
    put("x-stainless-timeout", HeaderValue::from_static("600"));
    put("anthropic-beta", header_value(&betas.join(","))?);
    put("anthropic-dangerous-direct-browser-access", HeaderValue::from_static("true"));
    put("anthropic-version", HeaderValue::from_static(API_VERSION));
    let mut auth = header_value(&format!("Bearer {}", token.expose()))?;
    auth.set_sensitive(true);
    put("authorization", auth);
    put("x-app", HeaderValue::from_static("cli"));
    put("connection", HeaderValue::from_static("keep-alive"));
    put("accept-encoding", HeaderValue::from_static(ACCEPT_ENCODING));
    Ok(h)
}

/// Plain API-key headers: no Claude Code identity at all.
fn api_key_headers(key: &Secret) -> Result<HeaderMap, String> {
    let mut h = HeaderMap::new();
    h.insert("accept", HeaderValue::from_static("text/event-stream"));
    h.insert("accept-encoding", HeaderValue::from_static(ACCEPT_ENCODING));
    h.insert("connection", HeaderValue::from_static("keep-alive"));
    h.insert("content-type", HeaderValue::from_static("application/json"));
    h.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
    h.insert("user-agent", header_value(concat!("review-master/", env!("CARGO_PKG_VERSION")))?);
    let mut value = header_value(key.expose())?;
    value.set_sensitive(true);
    h.insert("x-api-key", value);
    Ok(h)
}

/// OAuth-only request identity.
struct OAuthWire<'a> {
    version: &'a str,
    device_id: &'a str,
    account_id: Option<&'a str>,
    session: &'a str,
}

fn write_json<T: serde::Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) {
    serde_json::to_writer(&mut *out, value).expect("serializing JSON to memory cannot fail");
}

/// `{"type":"text","text":…[,"cache_control":…]}` in Claude Code's key order.
fn write_text_block(out: &mut Vec<u8>, text: &str, cache: Option<&Value>) {
    out.extend_from_slice(b"{\"type\":\"text\",\"text\":");
    write_json(out, text);
    if let Some(cache) = cache {
        out.extend_from_slice(b",\"cache_control\":");
        write_json(out, cache);
    }
    out.push(b'}');
}

/// Text of the first conversational user turn (omp's fingerprint seed).
fn first_user_text(messages: &[Value]) -> &str {
    for message in messages.iter().filter(|m| m["role"] == "user") {
        match &message["content"] {
            Value::String(text) if !text.trim().is_empty() => return text,
            Value::Array(blocks) => {
                let text = blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .find(|t| !t.trim().is_empty());
                if let Some(text) = text {
                    return text;
                }
                if blocks.iter().any(|b| b["type"] == "image") {
                    return "";
                }
            }
            _ => {}
        }
    }
    ""
}

/// `cc_version` suffix: SHA-256(salt + text[4] + text[7] + text[20] + version)[..3],
/// indexing JavaScript UTF-16 code units ("0" when absent) as Claude Code does.
fn billing_header(first_user_text: &str, version: &str) -> String {
    let units: Vec<u16> = first_user_text.encode_utf16().take(21).collect();
    let picked: Vec<u16> = [4, 7, 20].iter().map(|&i| units.get(i).copied().unwrap_or(u16::from(b'0'))).collect();
    let seed = String::from_utf16_lossy(&picked);
    let digest = hex(&Sha256::digest(format!("{BILLING_SALT}{seed}{version}"))[..2]);
    let suffix = &digest[..3];
    format!("{BILLING_PREFIX} cc_version={version}.{suffix}; cc_entrypoint=cli; {CCH_PLACEHOLDER};")
}

/// Puts `cache_control` on the last cacheable block; false when none was added.
fn decorate_message(message: &mut Value, cache: &Value) -> bool {
    match message.get_mut("content") {
        Some(content @ Value::String(_)) => {
            let text = content.take();
            *content = json!([{ "type": "text", "text": text, "cache_control": cache }]);
            true
        }
        Some(Value::Array(blocks)) => {
            for block in blocks.iter_mut().rev() {
                let kind = block["type"].as_str().unwrap_or("");
                // Anthropic rejects cache_control on generated reasoning and control blocks.
                if matches!(kind, "thinking" | "redacted_thinking" | "fallback" | "tool_addition" | "tool_removal") {
                    continue;
                }
                let Some(map) = block.as_object_mut() else { return false };
                if map.get("cache_control").is_some_and(|c| !c.is_null()) {
                    return false;
                }
                map.insert("cache_control".into(), cache.clone());
                return true;
            }
            false
        }
        _ => false,
    }
}

/// Decorated copies of the newest two messages, within the remaining budget.
fn message_breakpoints(messages: &[Value], cache: &Value, budget: usize) -> Vec<(usize, Value)> {
    let mut out = Vec::new();
    for index in (0..messages.len()).rev().take(2) {
        if out.len() >= budget {
            break;
        }
        let mut message = messages[index].clone();
        if decorate_message(&mut message, cache) {
            out.push((index, message));
        }
    }
    out
}

/// Serializes the request in omp's field order: model, messages, system, tools,
/// metadata, max_tokens, stream. Only the ≤2 decorated messages are cloned.
fn build_body(
    model: &str, max_tokens: u32, system: &str, messages: &[Value], tools: &[Value], oauth: Option<&OAuthWire>,
) -> Vec<u8> {
    // OAuth defaults to 1h retention (Claude Code subscriber policy); API keys use 5m.
    let cache = match oauth {
        Some(_) => json!({ "type": "ephemeral", "ttl": "1h" }),
        None => json!({ "type": "ephemeral" }),
    };
    let has_system = !system.trim().is_empty();
    let system_breakpoint = has_system || oauth.is_some();
    let head = usize::from(!tools.is_empty()) + usize::from(system_breakpoint);
    let decorated = message_breakpoints(messages, &cache, MAX_BREAKPOINTS - head);

    let mut out = Vec::with_capacity(64 * 1024);
    out.extend_from_slice(b"{\"model\":");
    write_json(&mut out, model);
    out.extend_from_slice(b",\"messages\":[");
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        match decorated.iter().find(|(i, _)| *i == index) {
            Some((_, copy)) => write_json(&mut out, copy),
            None => write_json(&mut out, message),
        }
    }
    out.push(b']');

    let mut placeholder = None;
    if system_breakpoint {
        out.extend_from_slice(b",\"system\":[");
        if let Some(wire) = oauth {
            // system[0]: billing header. Its placeholder offset is recorded here, so
            // the cch patch can never touch message or system prompt text.
            out.extend_from_slice(b"{\"type\":\"text\",\"text\":");
            let start = out.len();
            write_json(&mut out, &billing_header(first_user_text(messages), wire.version));
            placeholder = out[start..]
                .windows(CCH_PLACEHOLDER.len())
                .position(|w| w == CCH_PLACEHOLDER.as_bytes())
                .map(|p| start + p);
            out.extend_from_slice(b"},");
            // The head breakpoint sits on the last stable system block.
            write_text_block(&mut out, IDENTITY, (!has_system).then_some(&cache));
            if has_system {
                out.push(b',');
            }
        }
        if has_system {
            write_text_block(&mut out, system, Some(&cache));
        }
        out.push(b']');
    }

    if !tools.is_empty() || oauth.is_some() {
        out.extend_from_slice(b",\"tools\":[");
        for (index, tool) in tools.iter().enumerate() {
            if index > 0 {
                out.push(b',');
            }
            let mut wire = tool.clone();
            if let Some(map) = wire.as_object_mut() {
                let buffered = map.get("name").and_then(Value::as_str).is_some_and(|n| BUFFERED_INPUT_TOOLS.contains(&n));
                map.entry("eager_input_streaming").or_insert(Value::Bool(!buffered));
                if index + 1 == tools.len() && !map.contains_key("cache_control") {
                    map.insert("cache_control".into(), cache.clone());
                }
            }
            write_json(&mut out, &wire);
        }
        out.push(b']');
    }

    if let Some(wire) = oauth {
        let mut user_id = Vec::with_capacity(160);
        user_id.extend_from_slice(b"{\"device_id\":");
        write_json(&mut user_id, wire.device_id);
        user_id.extend_from_slice(b",\"session_id\":");
        write_json(&mut user_id, wire.session);
        if let Some(account) = wire.account_id.filter(|a| !a.is_empty()) {
            user_id.extend_from_slice(b",\"account_uuid\":");
            write_json(&mut user_id, account);
        }
        user_id.push(b'}');
        out.extend_from_slice(b",\"metadata\":{\"user_id\":");
        write_json(&mut out, std::str::from_utf8(&user_id).expect("JSON is UTF-8"));
        out.push(b'}');
    }
    out.extend_from_slice(format!(",\"max_tokens\":{max_tokens},\"stream\":true}}").as_bytes());

    if let Some(at) = placeholder {
        patch_cch(&mut out, at);
    }
    out
}

/// cch attestation: xxHash64 of the body with the placeholder in place, low 20
/// bits as 5 hex digits, written over the placeholder's zeros.
fn patch_cch(body: &mut [u8], placeholder: usize) {
    let hash = xxhash_rust::xxh64::xxh64(body, CCH_SEED);
    let cch = format!("{:05x}", hash & 0xfffff);
    body[placeholder + 4..placeholder + 9].copy_from_slice(cch.as_bytes());
}

fn http_error(status: u16, body: &str, request_id: Option<&str>, retry_after: Option<&str>, source: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let detail = match parsed.as_ref().map(|v| &v["error"]) {
        Some(error) if error.is_object() => format!(
            "{}: {}",
            error["type"].as_str().unwrap_or("error"),
            one_line_bounded(&redact(error["message"].as_str().unwrap_or("")), 500)
        ),
        _ => one_line_bounded(&redact(body), 300),
    };
    let mut message = match status {
        401 => format!("anthropic rejected the credential from {source} (HTTP 401 {detail}); sign in again with `review-master --anthropic-login` or fix the key"),
        403 => format!("anthropic denied the request (HTTP 403 {detail}); the credential from {source} may lack access to this model"),
        429 => {
            let wait = retry_after.map(|s| format!("; retry after {s}s")).unwrap_or_default();
            format!("anthropic rate or usage limit reached (HTTP 429 {detail}){wait}")
        }
        500..=599 => format!("anthropic API unavailable (HTTP {status} {detail}); retry later"),
        _ => format!("anthropic request failed (HTTP {status} {detail})"),
    };
    if let Some(id) = request_id {
        message.push_str(&format!(" [request-id {}]", one_line_bounded(id, 80)));
    }
    message
}

async fn read_stream(response: reqwest::Response, progress: &mut impl FnMut(String)) -> Result<Reply, String> {
    let mut body = std::pin::pin!(response.bytes_stream());
    let mut sse = SseBuffer::default();
    let mut reply = Accumulator::default();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("anthropic stream interrupted: {}", redact(&e.without_url().to_string())))?;
        sse.push(&chunk)?;
        while let Some(data) = sse.next_event()? {
            if reply.apply(&data, progress)? {
                return reply.finish();
            }
        }
    }
    reply.finish()
}

/// Server-sent-event framing over arbitrary byte chunks. Events are decoded only
/// once complete, so split UTF-8 sequences and split JSON are reassembled.
#[derive(Default)]
struct SseBuffer {
    pending: Vec<u8>,
    /// Start of the first line not yet known to be complete.
    scan_from: usize,
}

impl SseBuffer {
    fn push(&mut self, chunk: &[u8]) -> Result<(), String> {
        if self.pending.len() + chunk.len() > MAX_PENDING_SSE {
            return Err("anthropic stream event exceeds 32 MiB".into());
        }
        self.pending.extend_from_slice(chunk);
        Ok(())
    }

    /// Next event's `data` payload, if a complete event is buffered.
    fn next_event(&mut self) -> Result<Option<String>, String> {
        loop {
            let Some(offset) = self.pending[self.scan_from..].iter().position(|&b| b == b'\n') else {
                return Ok(None);
            };
            let line_end = self.scan_from + offset;
            let line = &self.pending[self.scan_from..line_end];
            if line.is_empty() || line == b"\r" {
                let data = event_data(&self.pending[..self.scan_from])?;
                self.pending.drain(..=line_end);
                self.scan_from = 0;
                if data.is_some() {
                    return Ok(data);
                }
            } else {
                self.scan_from = line_end + 1;
            }
        }
    }
}

fn event_data(event: &[u8]) -> Result<Option<String>, String> {
    let text = std::str::from_utf8(event).map_err(|_| "anthropic stream sent invalid UTF-8".to_owned())?;
    let mut data: Option<String> = None;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            let value = value.strip_prefix(' ').unwrap_or(value);
            match &mut data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => data = Some(value.to_owned()),
            }
        }
    }
    Ok(data)
}

struct Block {
    value: Value,
    input_json: String,
    open: bool,
}

#[derive(Default)]
struct Accumulator {
    blocks: Vec<Option<Block>>,
    stop_reason: Option<String>,
    started: bool,
    stopped: bool,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
}

fn append(value: &mut Value, key: &str, text: &str) {
    match value.get_mut(key) {
        Some(Value::String(s)) => s.push_str(text),
        _ => value[key] = Value::String(text.to_owned()),
    }
}

impl Accumulator {
    /// Usage fields are cumulative snapshots: a later present field replaces it.
    fn usage(&mut self, usage: &Value) {
        let slots = [
            ("input_tokens", &mut self.input),
            ("cache_read_input_tokens", &mut self.cache_read),
            ("cache_creation_input_tokens", &mut self.cache_write),
            ("output_tokens", &mut self.output),
        ];
        for (key, slot) in slots {
            if let Some(n) = usage[key].as_u64() {
                *slot = n;
            }
        }
    }

    fn open_block(&mut self, event: &Value) -> Result<&mut Block, String> {
        let index = event["index"].as_u64().ok_or("anthropic stream event without a block index")? as usize;
        match self.blocks.get_mut(index) {
            Some(Some(block)) if block.open => Ok(block),
            _ => Err(format!("anthropic stream referenced unopened content block {index}")),
        }
    }

    /// Applies one event; true once `message_stop` arrives.
    fn apply(&mut self, data: &str, progress: &mut impl FnMut(String)) -> Result<bool, String> {
        let mut event: Value =
            serde_json::from_str(data).map_err(|e| format!("anthropic stream sent malformed JSON: {e}"))?;
        match event["type"].as_str().unwrap_or("") {
            "message_start" => {
                self.started = true;
                self.usage(&event["message"]["usage"]);
            }
            "content_block_start" => {
                let index = event["index"].as_u64().ok_or("anthropic stream event without a block index")? as usize;
                let value = event["content_block"].take();
                if !value.is_object() || index >= MAX_BLOCKS {
                    return Err("anthropic stream sent an invalid content block".into());
                }
                if self.blocks.len() <= index {
                    self.blocks.resize_with(index + 1, || None);
                }
                if self.blocks[index].is_some() {
                    return Err(format!("anthropic stream reopened content block {index}"));
                }
                self.blocks[index] = Some(Block { value, input_json: String::new(), open: true });
            }
            "content_block_delta" => {
                let block = self.open_block(&event)?;
                let delta = &event["delta"];
                let text = |key: &str| delta[key].as_str().unwrap_or("");
                match delta["type"].as_str().unwrap_or("") {
                    "text_delta" => append(&mut block.value, "text", text("text")),
                    "thinking_delta" => append(&mut block.value, "thinking", text("thinking")),
                    "signature_delta" => block.value["signature"] = Value::String(text("signature").to_owned()),
                    "input_json_delta" => block.input_json.push_str(text("partial_json")),
                    "citations_delta" => match block.value.get_mut("citations") {
                        Some(Value::Array(list)) => list.push(delta["citation"].clone()),
                        _ => block.value["citations"] = json!([delta["citation"].clone()]),
                    },
                    _ => {}
                }
            }
            "content_block_stop" => {
                let block = self.open_block(&event)?;
                block.open = false;
                if !block.input_json.is_empty() {
                    block.value["input"] = serde_json::from_str(&block.input_json)
                        .map_err(|e| format!("anthropic streamed invalid tool input JSON: {e}"))?;
                    block.input_json = String::new();
                }
                match block.value["type"].as_str().unwrap_or("") {
                    "text" => {
                        let text = block.value["text"].as_str().unwrap_or("").trim();
                        if !text.is_empty() && !text.starts_with('{') {
                            progress(format!("› {}", one_line(text, 300)));
                        }
                    }
                    "thinking" => {
                        let text = block.value["thinking"].as_str().unwrap_or("");
                        if !text.trim().is_empty() {
                            progress(format!("… {}", one_line(text, 300)));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_owned());
                }
                self.usage(&event["usage"]);
            }
            "message_stop" => {
                self.stopped = true;
                return Ok(true);
            }
            "error" => {
                let error = &event["error"];
                let kind = error["type"].as_str().unwrap_or("error");
                let hint = if kind == "overloaded_error" { "; Anthropic is overloaded, retry later" } else { "" };
                return Err(format!(
                    "anthropic stream error ({kind}): {}{hint}",
                    one_line_bounded(&redact(error["message"].as_str().unwrap_or("")), 500)
                ));
            }
            _ => {} // ping and future event types
        }
        Ok(false)
    }

    fn finish(self) -> Result<Reply, String> {
        if !self.started || !self.stopped {
            return Err("anthropic stream ended before message_stop (response truncated)".into());
        }
        let mut content = Vec::with_capacity(self.blocks.len());
        for (index, block) in self.blocks.into_iter().enumerate() {
            match block {
                Some(Block { value, open: false, .. }) => content.push(value),
                Some(_) => return Err(format!("anthropic stream ended inside content block {index}")),
                None => return Err(format!("anthropic stream skipped content block {index}")),
            }
        }
        Ok(Reply {
            content,
            stop_reason: self.stop_reason.unwrap_or_default(),
            input_tokens: self.input + self.cache_read + self.cache_write,
            cached_input_tokens: self.cache_read,
            output_tokens: self.output,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(events: &[Value]) -> Vec<u8> {
        let mut out = Vec::new();
        for event in events {
            out.extend_from_slice(format!("event: {}\r\ndata: {}\r\n\r\n", event["type"].as_str().unwrap(), event).as_bytes());
        }
        out
    }

    fn run(bytes: &[u8], chunk: usize) -> Result<Reply, String> {
        let mut buffer = SseBuffer::default();
        let mut acc = Accumulator::default();
        let mut noop = |_: String| {};
        for piece in bytes.chunks(chunk) {
            buffer.push(piece)?;
            while let Some(data) = buffer.next_event()? {
                if acc.apply(&data, &mut noop)? {
                    return acc.finish();
                }
            }
        }
        acc.finish()
    }

    fn full_stream() -> Vec<Value> {
        vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":50,"output_tokens":1}}}),
            json!({"type":"ping"}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Plan: héllo "}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"wörld 😀"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig=="}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Reading — ✓"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"_read","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"src/ma"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"in.rs\", \"limit\": 5}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_2","name":"_list","input":{}}}),
            json!({"type":"content_block_stop","index":3}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}),
            json!({"type":"message_stop"}),
        ]
    }

    #[test]
    fn stream_reassembles_split_utf8_and_json_into_complete_blocks() {
        let bytes = sse(&full_stream());
        for chunk in [1, 2, 3, 7, 64, bytes.len()] {
            let reply = run(&bytes, chunk).unwrap();
            assert_eq!(reply.stop_reason, "tool_use");
            assert_eq!(reply.content[0], json!({"type":"thinking","thinking":"Plan: héllo wörld 😀","signature":"sig=="}));
            assert_eq!(reply.content[1], json!({"type":"text","text":"Reading — ✓"}));
            assert_eq!(reply.content[2], json!({"type":"tool_use","id":"toolu_1","name":"_read","input":{"path":"src/main.rs","limit":5}}));
            assert_eq!(reply.content[3]["input"], json!({}));
        }
    }

    #[test]
    fn usage_counts_cache_reads_and_writes_once() {
        let reply = run(&sse(&full_stream()), 5).unwrap();
        assert_eq!(reply.input_tokens, 160);
        assert_eq!(reply.cached_input_tokens, 100);
        assert_eq!(reply.output_tokens, 42);
        let mut events = full_stream();
        events[16] = json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7,"cache_read_input_tokens":120}});
        let reply = run(&sse(&events), 9).unwrap();
        assert_eq!((reply.input_tokens, reply.cached_input_tokens, reply.output_tokens), (180, 120, 7));
    }

    #[test]
    fn truncated_error_and_malformed_streams_fail() {
        let events = full_stream();
        let truncated = run(&sse(&events[..16]), 4).unwrap_err();
        assert!(truncated.contains("truncated"), "{truncated}");
        let mut open = events[..12].to_vec();
        open.push(json!({"type":"message_stop"}));
        assert!(run(&sse(&open), 4).unwrap_err().contains("inside content block"));
        let mut partial = sse(&events);
        partial.truncate(partial.len() - 3);
        assert!(run(&partial, 11).is_err());
        let error = vec![
            events[0].clone(),
            json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
        ];
        let err = run(&sse(&error), 3).unwrap_err();
        assert!(err.contains("overloaded_error") && err.contains("Overloaded"), "{err}");
        let mut bad_tool = events[..11].to_vec();
        bad_tool.push(json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}));
        bad_tool.push(json!({"type":"content_block_stop","index":2}));
        assert!(run(&sse(&bad_tool), 8).unwrap_err().contains("invalid tool input"));
        assert!(run(b"data: {not json}\n\n", 3).unwrap_err().contains("malformed"));
        assert!(run(b"data: \xff\xfe\n\n", 3).unwrap_err().contains("UTF-8"));
    }

    #[test]
    fn billing_fingerprint_indexes_utf16_code_units() {
        // Reference suffixes computed with Node's JS string indexing + SHA-256.
        let cases = [
            ("Explain the architecture of this repo", "921"),
            ("héllo wörld 😀 emoji text here!!", "f73"),
            ("ab😀😀😀xyz", "235"),
        ];
        for (text, suffix) in cases {
            assert_eq!(
                billing_header(text, "2.1.280"),
                format!("x-anthropic-billing-header: cc_version=2.1.280.{suffix}; cc_entrypoint=cli; cch=00000;")
            );
        }
    }

    fn sample_messages(first: &str) -> Vec<Value> {
        vec![
            json!({"role":"user","content":[{"type":"text","text":first}]}),
            json!({"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"s"},{"type":"tool_use","id":"t1","name":"_read","input":{"path":"a"}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"cch=00000 x-anthropic-billing-header:"}]}),
            json!({"role":"assistant","content":[{"type":"thinking","thinking":"only","signature":"s"}]}),
        ]
    }

    fn sample_tools() -> Vec<Value> {
        vec![
            json!({"name":"_read","description":"r","input_schema":{"type":"object"}}),
            json!({"name":"_answer","description":"a","input_schema":{"type":"object"}}),
        ]
    }

    #[test]
    fn oauth_body_patches_only_the_billing_placeholder() {
        let first = r#"Look at "system":[{"type":"text","text":"x-anthropic-billing-header: cch=00000;"#;
        let messages = sample_messages(first);
        let wire = OAuthWire { version: "2.1.280", device_id: "dev", account_id: Some("acc"), session: "sess" };
        let body = build_body("claude-sonnet-5-5", 128_000, "SYSTEM", &messages, &sample_tools(), Some(&wire));
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.starts_with("{\"model\":\"claude-sonnet-5-5\",\"messages\":["));
        let marker = "\"system\":[{\"type\":\"text\",\"text\":\"x-anthropic-billing-header:";
        let at = text.find(marker).unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        // User text and tool results keep their literal placeholders.
        assert_eq!(parsed["messages"][0]["content"][0]["text"], first);
        assert_eq!(parsed["messages"][2]["content"][0]["content"], "cch=00000 x-anthropic-billing-header:");
        let billing = parsed["system"][0]["text"].as_str().unwrap();
        let cch = billing.rsplit("cch=").next().unwrap().trim_end_matches(';');
        assert_eq!(cch.len(), 5);
        // Recompute: the hash covers the body with the placeholder zeros in place.
        let offset = at + text[at..].find("cch=").unwrap() + 4;
        let mut original = body.clone();
        original[offset..offset + 5].copy_from_slice(b"00000");
        assert_eq!(format!("{:05x}", xxhash_rust::xxh64::xxh64(&original, CCH_SEED) & 0xfffff), cch);
        assert_eq!(parsed["system"][1], json!({"type":"text","text":IDENTITY}));
        assert_eq!(parsed["system"][2]["cache_control"], json!({"type":"ephemeral","ttl":"1h"}));
        let user_id: Value = serde_json::from_str(parsed["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(user_id, json!({"device_id":"dev","session_id":"sess","account_uuid":"acc"}));
        assert_eq!(parsed["max_tokens"], 128_000);
        assert_eq!(parsed["stream"], true);
        assert!(parsed.get("thinking").is_none());
    }

    #[test]
    fn caching_stays_within_four_breakpoints() {
        let messages = sample_messages("hello");
        let wire = OAuthWire { version: "2.1.280", device_id: "dev", account_id: None, session: "sess" };
        for oauth in [Some(&wire), None] {
            let body = build_body("claude-sonnet-5-5", 128_000, "SYSTEM", &messages, &sample_tools(), oauth);
            let text = std::str::from_utf8(&body).unwrap();
            assert!(text.matches("\"cache_control\"").count() <= MAX_BREAKPOINTS);
            let parsed: Value = serde_json::from_slice(&body).unwrap();
            assert!(parsed["tools"][1]["cache_control"].is_object());
            assert!(parsed["tools"][0].get("cache_control").is_none());
            assert_eq!(parsed["tools"][0]["eager_input_streaming"], true);
            assert_eq!(parsed["tools"][1]["eager_input_streaming"], false);
            // Newest message is thinking-only: no breakpoint there; the tool result gets one.
            assert!(parsed["messages"][3]["content"][0].get("cache_control").is_none());
            assert!(parsed["messages"][2]["content"][0]["cache_control"].is_object());
            assert!(parsed["messages"][0]["content"][0].get("cache_control").is_none());
            if oauth.is_none() {
                assert!(parsed.get("metadata").is_none());
                assert!(!text.contains("x-anthropic-billing-header: cc_version") && !text.contains(IDENTITY));
                assert_eq!(parsed["system"], json!([{"type":"text","text":"SYSTEM","cache_control":{"type":"ephemeral"}}]));
            }
        }
        // Caller history is never mutated.
        assert!(messages[2]["content"][0].get("cache_control").is_none());
        let strings = vec![json!({"role":"user","content":"a"}), json!({"role":"user","content":"b"})];
        let body = build_body("m", 1, "", &strings, &[], None);
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["messages"][1]["content"][0]["cache_control"], json!({"type":"ephemeral"}));
        assert!(parsed.get("system").is_none() && parsed.get("tools").is_none());
    }

    #[test]
    fn headers_isolate_oauth_identity_from_api_keys() {
        let oauth = oauth_headers(&Secret::for_test("tok"), "sess", "2.1.280", true).unwrap();
        assert_eq!(oauth["user-agent"], "claude-cli/2.1.280 (external, cli)");
        assert_eq!(oauth["x-claude-code-session-id"], "sess");
        assert!(oauth["anthropic-beta"].to_str().unwrap().starts_with("claude-code-20250219,oauth-2025-04-20"));
        assert_eq!(oauth["authorization"], "Bearer tok");
        assert!(oauth["authorization"].is_sensitive());
        let key = api_key_headers(&Secret::for_test("key")).unwrap();
        assert_eq!(key["x-api-key"], "key");
        for name in ["authorization", "x-app", "anthropic-beta", "x-claude-code-session-id", "x-stainless-os"] {
            assert!(key.get(name).is_none(), "{name}");
        }
        assert!(!key["user-agent"].to_str().unwrap().contains("claude"));
    }

    #[test]
    fn version_floor_is_adopted_only_when_newer() {
        let text = r#"{"error":{"type":"invalid_request_error","message":"claude_code_version_too_old: Claude Code version 2.2.0 or newer is required"}}"#;
        assert_eq!(required_version(text, "2.1.280").as_deref(), Some("2.2.0"));
        assert_eq!(required_version(text, "2.2.0"), None);
        assert_eq!(required_version("version 9.9.9 or newer is required", "2.1.280"), None);
        assert_eq!(max_output_tokens("claude-sonnet-5-5"), 128_000);
        assert_eq!(max_output_tokens("claude-opus-4-6"), 32_768);
    }

    #[test]
    fn http_errors_classify_rate_limits_separately_from_auth() {
        let rate = http_error(429, r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#, Some("req_1"), Some("30"), "x");
        assert!(rate.contains("rate or usage limit") && rate.contains("retry after 30s") && !rate.contains("credential"));
        let auth = http_error(401, r#"{"error":{"type":"authentication_error","message":"bad sk-ant-api03-SECRET"}}"#, None, None, "ANTHROPIC_API_KEY");
        assert!(auth.contains("rejected the credential") && !auth.contains("SECRET"));
    }
}
