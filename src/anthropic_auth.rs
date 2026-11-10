//! Credentials for the native Anthropic engine.
//!
//! Precedence (first match wins, mirroring oh-my-pi's env order):
//! 1. `ANTHROPIC_OAUTH_TOKEN`: a subscription OAuth access token (never refreshed).
//! 2. `ANTHROPIC_API_KEY`: a Console API key (an `sk-ant-oat…` value is OAuth, like omp).
//! 3. review-master's own OAuth login in `$XDG_CONFIG_HOME/review-master/anthropic.json`
//!    (fallback `$HOME/.config/review-master/anthropic.json`), written by `--anthropic-login`.
//!
//! Claude Code's credentials are never read or rotated: refreshing them here would
//! invalidate the refresh token that app still holds.
//!
//! The OAuth client constants, authorize parameters, token request shapes, callback
//! port/path and 5-minute expiry skew follow oh-my-pi
//! (`packages/catalog/src/compat/rules/auth/anthropic.kdl`,
//! `packages/ai/src/registry/oauth/*`, MIT License, Copyright (c) Mario Zechner and
//! oh-my-pi contributors).

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

/// Public Claude OAuth client id, base64 like upstream so secret scanners stay quiet.
const CLIENT_ID_B64: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const BOOTSTRAP_URL: &str = "https://api.anthropic.com/api/claude_cli/bootstrap";
const BOOTSTRAP_MODEL: &str = "claude-opus-4-8";
const SCOPES: &[&str] = &[
    "org:create_api_key", "user:profile", "user:inference", "user:sessions:claude_code",
    "user:mcp_servers", "user:file_upload",
];
const CALLBACK_PORT: u16 = 54545;
const CALLBACK_PATH: &str = "/callback";
const REDIRECT_URI: &str = "http://localhost:54545/callback";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Stored expiry is `expires_in` minus this skew, so tokens refresh 5 minutes early.
const EXPIRY_SKEW_MS: u64 = 300_000;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const ERROR_BODY_LIMIT: usize = 16 * 1024;

/// A credential string that never prints itself.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str { &self.0 }

    #[cfg(test)]
    pub fn for_test(value: &str) -> Secret { Secret(value.to_owned()) }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("Secret(<redacted>)") }
}

/// Persisted review-master OAuth login. Deliberately not `Debug`.
#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    access: String,
    refresh: String,
    /// Unix milliseconds, already reduced by [`EXPIRY_SKEW_MS`].
    expires: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    org_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    org_name: Option<String>,
    /// Random per-login installation id; seeds the metadata device id.
    install_id: String,
}

enum Kind {
    ApiKey(Secret),
    EnvOAuth(Secret),
    Stored { path: PathBuf, cache: Box<Mutex<Stored>> },
}

/// Selected Anthropic credential source.
pub struct Auth {
    kind: Kind,
    install_id: String,
    account_id: Option<String>,
}

impl Auth {
    /// Picks the credential source; reads (never refreshes) the login file.
    pub fn resolve() -> Result<Auth, String> {
        if let Some(token) = env_secret("ANTHROPIC_OAUTH_TOKEN") {
            return Ok(Auth { kind: Kind::EnvOAuth(token), install_id: uuid::Uuid::new_v4().to_string(), account_id: None });
        }
        if let Some(key) = env_secret("ANTHROPIC_API_KEY") {
            // Same detection as omp's `isOAuthToken`.
            let kind = if key.expose().contains("sk-ant-oat") { Kind::EnvOAuth(key) } else { Kind::ApiKey(key) };
            return Ok(Auth { kind, install_id: uuid::Uuid::new_v4().to_string(), account_id: None });
        }
        let path = credentials_path()?;
        match read_stored(&path)? {
            Some(stored) => Ok(Auth {
                install_id: stored.install_id.clone(),
                account_id: stored.account_id.clone(),
                kind: Kind::Stored { path, cache: Box::new(Mutex::new(stored)) },
            }),
            None => Err(format!(
                "no Anthropic credentials: set ANTHROPIC_API_KEY or ANTHROPIC_OAUTH_TOKEN, or run `review-master --anthropic-login` (stores {})",
                path.display()
            )),
        }
    }

    /// OAuth credentials use Claude Code's wire identity; API keys never do.
    pub fn is_oauth(&self) -> bool { !matches!(self.kind, Kind::ApiKey(_)) }

    pub fn install_id(&self) -> &str { &self.install_id }

    pub fn account_id(&self) -> Option<&str> { self.account_id.as_deref() }

    /// Where the credential came from, for error hints.
    pub fn source(&self) -> &'static str {
        match self.kind {
            Kind::ApiKey(_) => "ANTHROPIC_API_KEY",
            Kind::EnvOAuth(_) => "the environment OAuth token",
            Kind::Stored { .. } => "the review-master OAuth login (`review-master --anthropic-login`)",
        }
    }

    /// Current key or access token, refreshing an expired stored login first.
    pub async fn bearer(&self, client: &reqwest::Client) -> Result<Secret, String> {
        match &self.kind {
            Kind::ApiKey(s) | Kind::EnvOAuth(s) => Ok(s.clone()),
            Kind::Stored { path, cache } => {
                let mut cached = cache.lock().await;
                if now_ms() < cached.expires {
                    return Ok(Secret(cached.access.clone()));
                }
                *cached = refresh_detached(client, path, None).await?;
                Ok(Secret(cached.access.clone()))
            }
        }
    }

    /// After a 401: refresh a stored login whose `rejected` token is still current.
    /// Returns whether a different token is now available.
    pub async fn refresh_after_rejection(&self, client: &reqwest::Client, rejected: &Secret) -> Result<bool, String> {
        let Kind::Stored { path, cache } = &self.kind else { return Ok(false) };
        let mut cached = cache.lock().await;
        if cached.access != rejected.0 {
            return Ok(true);
        }
        *cached = refresh_detached(client, path, Some(rejected.0.clone())).await?;
        Ok(cached.access != rejected.0)
    }
}

fn env_secret(name: &str) -> Option<Secret> {
    std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()).map(Secret)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn client_id() -> String {
    String::from_utf8(STANDARD.decode(CLIENT_ID_B64).expect("valid client id")).expect("utf-8 client id")
}

/// `<config dir>/anthropic.json` (see [`crate::config::dir`]).
pub fn credentials_path() -> Result<PathBuf, String> {
    Ok(crate::config::dir()?.join("anthropic.json"))
}

fn read_stored(path: &Path) -> Result<Option<Stored>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("invalid {}: {e}; run `review-master --anthropic-login` again", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

/// Atomic private write: 0600 temp file in the same directory, fsync, rename.
fn write_stored(path: &Path, stored: &Stored) -> Result<(), String> {
    let dir = path.parent().ok_or("credentials path has no parent")?;
    fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    let tmp = dir.join(format!(".anthropic.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> std::io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(stored).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Cross-process lock serializing refreshes and logins (released when dropped).
struct FileLock(#[allow(dead_code)] fs::File);

async fn lock_credentials(path: &Path) -> Result<FileLock, String> {
    let dir = path.parent().ok_or("credentials path has no parent")?;
    fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let lock_path = dir.join("anthropic.json.lock");
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(&lock_path).map_err(|e| format!("opening {}: {e}", lock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let deadline = tokio::time::Instant::now() + LOCK_TIMEOUT;
        loop {
            // SAFETY: flock on a descriptor owned by `file` for the duration of the call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) && err.kind() != std::io::ErrorKind::Interrupted {
                return Err(format!("locking {}: {err}", lock_path.display()));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("timed out waiting for {} (another review-master refreshing?)", lock_path.display()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Ok(FileLock(file))
}

/// Refreshes in a spawned task so dropping the caller (turn cancellation) cannot
/// lose a rotated refresh token between the server response and the file write.
async fn refresh_detached(client: &reqwest::Client, path: &Path, rejected: Option<String>) -> Result<Stored, String> {
    let task = tokio::spawn(refresh_stored(client.clone(), path.to_owned(), rejected));
    task.await.map_err(|e| format!("anthropic token refresh task failed: {e}"))?
}

async fn refresh_stored(client: reqwest::Client, path: PathBuf, rejected: Option<String>) -> Result<Stored, String> {
    let _lock = lock_credentials(&path).await?;
    // Another process may have refreshed (or re-logged in) while we waited.
    let current = read_stored(&path)?
        .ok_or_else(|| format!("{} disappeared; run `review-master --anthropic-login`", path.display()))?;
    let forced = rejected.as_deref() == Some(current.access.as_str());
    if !forced && now_ms() < current.expires {
        return Ok(current);
    }
    if current.refresh.is_empty() {
        return Err("stored Anthropic login has no refresh token; run `review-master --anthropic-login`".into());
    }
    let body = json!({
        "grant_type": "refresh_token",
        "client_id": client_id(),
        "refresh_token": current.refresh,
    });
    let response = post_token(&client, &body, true).await?;
    let updated = credentials_from(&response, Some(&current), current.install_id.clone(), now_ms())?;
    write_stored(&path, &updated)?;
    Ok(updated)
}

async fn post_token(client: &reqwest::Client, body: &Value, refresh: bool) -> Result<Value, String> {
    let what = if refresh { "token refresh" } else { "token exchange" };
    let mut request = client.post(TOKEN_URL).timeout(TOKEN_TIMEOUT).header("Content-Type", "application/json");
    if refresh {
        // Claude Code sends these on refresh but not on the initial code exchange.
        request = request
            .header("anthropic-beta", OAUTH_BETA)
            .header("User-Agent", format!("anthropic-sdk-typescript/{} userOAuthProvider", crate::anthropic_wire::CLAUDE_CODE_SDK_VERSION));
    }
    let response = request
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("anthropic {what} request failed: {}", redact(&e.without_url().to_string())))?;
    let status = response.status();
    let text = read_bounded(response, ERROR_BODY_LIMIT).await;
    if !status.is_success() {
        let detail = one_line_bounded(&redact(&text), 500);
        let hint = match status.as_u16() {
            429 => "rate limited; retry later",
            500..=599 => "Anthropic auth service unavailable; retry later",
            _ => "run `review-master --anthropic-login` to sign in again",
        };
        return Err(format!("anthropic {what} failed: HTTP {status}: {detail} ({hint})"));
    }
    serde_json::from_str(&text).map_err(|_| format!("anthropic {what} returned a non-JSON response"))
}

/// Projects a token response; refreshes keep a missing refresh token and the
/// login-time organization (omp never remaps org on refresh).
fn credentials_from(body: &Value, previous: Option<&Stored>, install_id: String, now: u64) -> Result<Stored, String> {
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let access = text(&body["access_token"]).ok_or("anthropic token response is missing access_token")?;
    let seconds = body["expires_in"].as_f64().filter(|s| s.is_finite()).ok_or("anthropic token response is missing expires_in")?;
    let expires = (now as f64 + seconds * 1000.0 - EXPIRY_SKEW_MS as f64).max(0.0) as u64;
    let refresh = text(&body["refresh_token"]).or_else(|| previous.map(|p| p.refresh.clone())).unwrap_or_default();
    let (org_id, org_name) = match previous {
        Some(p) => (p.org_id.clone(), p.org_name.clone()),
        None => (text(&body["organization"]["uuid"]), text(&body["organization"]["name"])),
    };
    Ok(Stored {
        access,
        refresh,
        expires,
        account_id: text(&body["account"]["uuid"]).or_else(|| previous.and_then(|p| p.account_id.clone())),
        email: text(&body["account"]["email_address"]).or_else(|| previous.and_then(|p| p.email.clone())),
        org_id,
        org_name,
        install_id,
    })
}

/// Reads at most `limit` bytes of a response body as lossy UTF-8.
pub(crate) async fn read_bounded(mut response: reqwest::Response, limit: usize) -> String {
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = limit - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if bytes.len() >= limit {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Masks anything token-shaped before text reaches an error message.
pub(crate) fn redact(text: &str) -> String {
    use std::sync::LazyLock;
    static KEY: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"sk-ant-[A-Za-z0-9_\-]+").unwrap());
    static FIELD: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#""(access_token|refresh_token|id_token|code|code_verifier)"\s*:\s*"[^"]*""#).unwrap()
    });
    let masked = KEY.replace_all(text, "sk-ant-[redacted]");
    FIELD.replace_all(&masked, "\"$1\":\"[redacted]\"").into_owned()
}

pub(crate) fn one_line_bounded(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

// ---------------------------------------------------------------- login

/// Interactive Claude Pro/Max OAuth login (authorization code + PKCE).
///
/// Opens the browser (also prints the URL), accepts the redirect on
/// `localhost:54545/callback`, or a pasted redirect URL / `code#state`, checks the
/// state, exchanges the code and stores the credential privately.
pub async fn login() -> Result<(), String> {
    let path = credentials_path()?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let state = uuid::Uuid::new_v4().simple().to_string();
    let listeners = bind_callback().await?;
    let url = authorize_url(&client_id(), &challenge, &state);

    eprintln!("Sign in to Anthropic (Claude Pro/Max) in your browser:\n\n  {url}\n");
    open_browser(&url);
    eprintln!(
        "Waiting up to {} minutes for the browser to return to {REDIRECT_URI}.\n\
         If the browser cannot reach this machine, paste the final redirect URL or the code#state \
         shown by Anthropic here and press Enter (input is hidden). Ctrl-C cancels.",
        LOGIN_TIMEOUT.as_secs() / 60
    );
    let code = wait_for_code(listeners, &state).await?;
    eprintln!("Exchanging the authorization code…");

    let body = json!({
        "grant_type": "authorization_code",
        "client_id": client_id(),
        "code": code,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": verifier,
        "state": state,
    });
    let response = post_token(&client, &body, false).await?;
    let install_id = read_stored(&path)
        .ok()
        .flatten()
        .map(|s| s.install_id)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut stored = credentials_from(&response, None, install_id, now_ms())?;
    if stored.account_id.is_none() || stored.email.is_none() || stored.org_id.is_none() {
        if let Some(identity) = bootstrap_identity(&client, &stored.access).await {
            stored.account_id = stored.account_id.or(identity.0);
            stored.email = stored.email.or(identity.1);
            stored.org_id = stored.org_id.or(identity.2);
            stored.org_name = stored.org_name.or(identity.3);
        }
    }
    let _lock = lock_credentials(&path).await?;
    write_stored(&path, &stored)?;
    let who = match (&stored.email, &stored.org_name) {
        (Some(email), Some(org)) => format!(" as {email} ({org})"),
        (Some(email), None) => format!(" as {email}"),
        _ => String::new(),
    };
    eprintln!("Signed in to Anthropic{who}. Credentials saved to {} (private).", path.display());
    Ok(())
}

/// Account/org identity from Claude Code's CLI bootstrap; best-effort.
async fn bootstrap_identity(
    client: &reqwest::Client, access: &str,
) -> Option<(Option<String>, Option<String>, Option<String>, Option<String>)> {
    let url = format!("{BOOTSTRAP_URL}?entrypoint=cli&model={BOOTSTRAP_MODEL}");
    let response = client
        .get(url)
        .timeout(TOKEN_TIMEOUT)
        .header("Accept", "application/json, text/plain, */*")
        .header("Authorization", format!("Bearer {access}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", format!("claude-code/{}", crate::anthropic_wire::claude_code_version()))
        .header("anthropic-beta", OAUTH_BETA)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = serde_json::from_str(&read_bounded(response, 256 * 1024).await).ok()?;
    let account = &body["oauth_account"];
    let text = |key: &str| account[key].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    Some((text("account_uuid"), text("account_email"), text("organization_uuid"), text("organization_name")))
}

/// 96 random bytes, base64url: a 128-character RFC 7636 verifier (as omp).
fn pkce_verifier() -> String {
    // uuid v4 draws from the OS CSPRNG; six of them give 732 random bits.
    let bytes: Vec<u8> = (0..6).flat_map(|_| *uuid::Uuid::new_v4().as_bytes()).collect();
    URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorize_url(client_id: &str, challenge: &str, state: &str) -> String {
    let scope = SCOPES.join(" ");
    let params = [
        ("client_id", client_id),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", &scope),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("code", "true"),
    ];
    let query: Vec<String> = params.iter().map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v))).collect();
    format!("{AUTHORIZE_URL}?{}", query.join("&"))
}

/// `application/x-www-form-urlencoded` like WHATWG URLSearchParams.
fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push((hi * 16 + lo) as u8);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (form_decode(k) == name).then(|| form_decode(v))
    })
}

fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    match std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(_) => eprintln!("(Could not start {program}; open the URL above manually.)"),
    }
}

/// Binds IPv4 and IPv6 loopback so `localhost` reaches us whichever family the
/// browser resolves first (omp binds both for the same reason).
async fn bind_callback() -> Result<Vec<TcpListener>, String> {
    let busy = |e: std::io::Error| {
        format!(
            "cannot listen on localhost:{CALLBACK_PORT} for the OAuth callback ({e}); \
             Anthropic only accepts this registered redirect port, so stop the process using it and retry"
        )
    };
    let mut listeners = vec![TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await.map_err(busy)?];
    match TcpListener::bind(("::1", CALLBACK_PORT)).await {
        Ok(v6) => listeners.push(v6),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return Err(busy(e)),
        Err(_) => {} // No IPv6 loopback on this host: IPv4 serves alone.
    }
    Ok(listeners)
}

#[derive(Debug, PartialEq)]
enum Callback {
    Code(String),
    Denied(String),
    Rejected(&'static str),
    NotFound,
}

/// Classifies one callback request target (`/callback?code=…&state=…`).
fn classify_callback(target: &str, expected_state: &str) -> Callback {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
        return Callback::NotFound;
    }
    let state = query_param(query, "state").unwrap_or_default();
    if let Some(error) = query_param(query, "error").filter(|e| !e.is_empty()) {
        // Only a redirect carrying our nonce can end the login; any local process
        // could forge an error without it.
        if state != expected_state {
            return Callback::Rejected("state mismatch");
        }
        let description = query_param(query, "error_description").filter(|d| !d.is_empty()).unwrap_or(error);
        return Callback::Denied(one_line_bounded(&description, 300));
    }
    let Some(code) = query_param(query, "code").filter(|c| !c.is_empty()) else {
        return Callback::Rejected("missing authorization code");
    };
    if state != expected_state {
        return Callback::Rejected("state mismatch");
    }
    Callback::Code(code)
}

async fn serve_callback(listener: TcpListener, state: String, tx: mpsc::UnboundedSender<Event>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let state = state.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let handled = tokio::time::timeout(Duration::from_secs(10), async {
                let mut head = Vec::with_capacity(1024);
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
                    let n = stream.read(&mut buf).await.ok()?;
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&buf[..n]);
                }
                let line = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_owned();
                let mut parts = line.split(' ');
                let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let outcome = if method == "GET" { classify_callback(target, &state) } else { Callback::NotFound };
                let (status, message) = match &outcome {
                    Callback::Code(_) => ("200 OK", "Anthropic sign-in complete. You can close this tab and return to review-master."),
                    Callback::Denied(_) => ("400 Bad Request", "Anthropic sign-in was denied. Return to review-master."),
                    Callback::Rejected(why) => ("400 Bad Request", *why),
                    Callback::NotFound => ("404 Not Found", "Not found"),
                };
                let html = format!("<!doctype html><meta charset=utf-8><title>review-master</title><p>{message}</p>");
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{html}",
                    html.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
                Some(outcome)
            })
            .await;
            match handled {
                Ok(Some(Callback::Code(code))) => {
                    let _ = tx.send(Event::Callback(Ok(code)));
                }
                Ok(Some(Callback::Denied(why))) => {
                    let _ = tx.send(Event::Callback(Err(format!("Anthropic authorization failed: {why}"))));
                }
                _ => {}
            }
        });
    }
}

enum Event {
    Callback(Result<String, String>),
    Manual(String),
}

/// Parses a pasted redirect URL, query string, or `code#state`. Errors never
/// include the pasted text.
fn parse_manual(input: &str, expected_state: &str) -> Result<String, &'static str> {
    let value = input.trim();
    if value.is_empty() {
        return Err("empty input");
    }
    let (code, state) = if value.starts_with("http://") || value.starts_with("https://") {
        let rest = value.split_once('#').map_or(value, |(before, _)| before);
        let query = rest.split_once('?').map_or("", |(_, q)| q);
        (query_param(query, "code"), query_param(query, "state"))
    } else if value.contains("code=") {
        let query = value.trim_start_matches(['?', '#']);
        (query_param(query, "code"), query_param(query, "state"))
    } else {
        let (code, state) = value.split_once('#').map_or((value, None), |(c, s)| (c, Some(s.to_owned())));
        (Some(code.to_owned()), state)
    };
    let code = code.filter(|c| !c.is_empty()).ok_or("no authorization code found in the pasted text")?;
    match state.filter(|s| !s.is_empty()) {
        None => Err("the pasted text has no state; paste the full redirect URL or code#state"),
        Some(s) if s != expected_state => Err("state mismatch (is this from an older login attempt?); ignored"),
        Some(_) => Ok(code),
    }
}

async fn wait_for_code(listeners: Vec<TcpListener>, state: &str) -> Result<String, String> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let servers: Vec<_> = listeners.into_iter().map(|l| tokio::spawn(serve_callback(l, state.to_owned(), tx.clone()))).collect();
    let _hidden = crate::term::EchoGuard::hide();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(Event::Manual(line)).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let deadline = tokio::time::sleep(LOGIN_TIMEOUT);
    tokio::pin!(deadline);
    let result = loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(Event::Callback(result)) => break result,
                Some(Event::Manual(line)) => match parse_manual(&line, state) {
                    Ok(code) => break Ok(code),
                    Err("empty input") => {}
                    Err(why) => eprintln!("{why}"),
                },
                None => break Err("OAuth callback listener stopped".into()),
            },
            _ = &mut deadline => break Err(format!("Anthropic login timed out after {} minutes", LOGIN_TIMEOUT.as_secs() / 60)),
            _ = tokio::signal::ctrl_c() => break Err("Anthropic login cancelled".into()),
        }
    };
    for server in servers {
        server.abort();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_and_verifier_shape() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let verifier = pkce_verifier();
        assert_eq!(verifier.len(), 128);
        assert_ne!(verifier, pkce_verifier());
        assert_eq!(client_id().len(), 36);
    }

    #[test]
    fn authorize_url_carries_pkce_state_and_encoded_scopes() {
        let url = authorize_url("cid", "chal", "st8");
        assert!(url.starts_with("https://claude.ai/oauth/authorize?client_id=cid&response_type=code&"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback"));
        assert!(url.contains("scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference+"));
        assert!(url.contains("code_challenge=chal&code_challenge_method=S256&state=st8&code=true"));
    }

    #[test]
    fn callback_requires_matching_state() {
        assert_eq!(classify_callback("/callback?code=abc&state=s1", "s1"), Callback::Code("abc".into()));
        assert_eq!(classify_callback("/callback?code=abc&state=evil", "s1"), Callback::Rejected("state mismatch"));
        assert_eq!(classify_callback("/callback?code=abc", "s1"), Callback::Rejected("state mismatch"));
        assert_eq!(classify_callback("/callback?state=s1", "s1"), Callback::Rejected("missing authorization code"));
        assert_eq!(classify_callback("/callback?error=access_denied&state=x", "s1"), Callback::Rejected("state mismatch"));
        assert_eq!(
            classify_callback("/callback?error=access_denied&error_description=User+said+no&state=s1", "s1"),
            Callback::Denied("User said no".into())
        );
        assert_eq!(classify_callback("/favicon.ico", "s1"), Callback::NotFound);
        assert_eq!(classify_callback("/callback?code=a%2Bb%3D&state=s1", "s1"), Callback::Code("a+b=".into()));
    }

    #[test]
    fn manual_input_accepts_url_query_or_code_hash_state_and_checks_state() {
        let ok = Ok("thecode".to_owned());
        assert_eq!(parse_manual("  thecode#s1 \n", "s1"), ok);
        assert_eq!(parse_manual("http://localhost:54545/callback?code=thecode&state=s1", "s1"), ok);
        assert_eq!(parse_manual("?code=thecode&state=s1", "s1"), ok);
        assert!(parse_manual("thecode#other", "s1").is_err());
        assert!(parse_manual("thecode", "s1").is_err());
        assert!(parse_manual("http://localhost:54545/callback?state=s1", "s1").is_err());
        let err = parse_manual("secretcode#other", "s1").unwrap_err();
        assert!(!err.contains("secretcode"));
    }

    #[tokio::test]
    async fn callback_server_ignores_forged_state_then_accepts_real_redirect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve_callback(listener, "good".into(), tx));
        let get = move |target: &'static str| async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        };
        assert!(get("/callback?code=x&state=bad").await.starts_with("HTTP/1.1 400"));
        assert!(get("/callback?code=real&state=good").await.starts_with("HTTP/1.1 200"));
        match rx.recv().await {
            Some(Event::Callback(Ok(code))) => assert_eq!(code, "real"),
            _ => panic!("expected the real code"),
        }
        server.abort();
    }

    #[test]
    fn token_projection_skews_expiry_and_keeps_refresh_and_org_on_refresh() {
        let login = json!({
            "access_token": "a1", "refresh_token": "r1", "expires_in": 3600,
            "account": {"uuid": "acc", "email_address": "me@x"},
            "organization": {"uuid": "org", "name": "Org"}
        });
        let first = credentials_from(&login, None, "inst".into(), 1_000_000).unwrap();
        assert_eq!(first.expires, 1_000_000 + 3_600_000 - 300_000);
        assert_eq!(first.org_id.as_deref(), Some("org"));
        let refresh = json!({"access_token": "a2", "expires_in": 60, "organization": {"uuid": "other"}});
        let second = credentials_from(&refresh, Some(&first), first.install_id.clone(), 0).unwrap();
        assert_eq!(second.refresh, "r1");
        assert_eq!(second.org_id.as_deref(), Some("org"));
        assert_eq!(second.account_id.as_deref(), Some("acc"));
        assert!(credentials_from(&json!({"access_token": "a"}), None, "i".into(), 0).is_err());
    }

    #[test]
    fn stored_credentials_are_private_and_atomic() {
        let dir = std::env::temp_dir().join(format!("review-master-auth-{}", uuid::Uuid::new_v4()));
        let path = dir.join("review-master").join("anthropic.json");
        let stored = credentials_from(&json!({"access_token": "a", "refresh_token": "r", "expires_in": 10}), None, "i".into(), 0).unwrap();
        write_stored(&path, &stored).unwrap();
        write_stored(&path, &stored).unwrap();
        assert_eq!(read_stored(&path).unwrap().unwrap().access, "a");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("anthropic.json")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn redaction_masks_tokens() {
        let text = r#"{"access_token":"abc","error":"bad sk-ant-oat01-XYZ_123"}"#;
        let masked = redact(text);
        assert!(!masked.contains("abc") && !masked.contains("XYZ_123"));
        assert_eq!(format!("{:?}", Secret("sk".into())), "Secret(<redacted>)");
    }
}
