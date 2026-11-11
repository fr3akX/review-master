//! `review-master --configure` (and the first interactive launch): choose the
//! engine and model, check that they are usable, and save the global configuration.

use crate::config::{self, Config};
use crate::engine::Kind;
use crate::term::{prompt, prompt_hidden};
use crate::{anthropic, anthropic_auth, openai};
use std::process::{Command, Stdio};

const ENGINES: [Kind; 4] = [Kind::Codex, Kind::Claude, Kind::Anthropic, Kind::OpenAi];
const MAX_LISTED_MODELS: usize = 40;

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

fn env_set(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.trim().is_empty())
}

fn describe(kind: Kind) -> &'static str {
    match kind {
        Kind::Codex => "Codex CLI (codex app-server)",
        Kind::Claude => "Claude Code CLI (claude -p)",
        Kind::Anthropic => "built-in agent: Claude subscription or Anthropic API key",
        Kind::OpenAi => "built-in agent: OpenAI-compatible server (OpenAI, OpenRouter, llama.cpp, vLLM, Ollama, …)",
    }
}

/// Whether an engine looks usable right now.
fn status(kind: Kind, cfg: &Config) -> String {
    match kind {
        Kind::Codex if !on_path("codex") => "not found on PATH".into(),
        Kind::Codex => {
            let logged_in = Command::new("codex")
                .args(["login", "status"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if logged_in { "found, logged in".into() } else { "found; run `codex login`".into() }
        }
        Kind::Claude => if on_path("claude") { "found".into() } else { "not found on PATH".into() },
        Kind::Anthropic if env_set("ANTHROPIC_API_KEY") || env_set("ANTHROPIC_OAUTH_TOKEN") => "credentials in the environment".into(),
        Kind::Anthropic if anthropic_signed_in() => "signed in".into(),
        Kind::Anthropic => "not signed in yet".into(),
        Kind::OpenAi => match cfg.openai_base_url.clone().or_else(|| std::env::var("OPENAI_BASE_URL").ok()) {
            Some(url) => format!("server {url}"),
            None => "needs a server URL".into(),
        },
    }
}

fn anthropic_signed_in() -> bool {
    anthropic_auth::credentials_path().is_ok_and(|p| p.is_file())
}

fn confirm(question: &str, default: bool) -> Result<bool, String> {
    loop {
        let answer = prompt(&format!("{question} [{}] ", if default { "Y/n" } else { "y/N" }))?.to_lowercase();
        match answer.as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("  answer y or n"),
        }
    }
}

/// A text value: Enter keeps `current`, `-` clears it.
fn text(question: &str, current: Option<&str>, empty_means: &str) -> Result<Option<String>, String> {
    let shown = current.unwrap_or(empty_means);
    let answer = prompt(&format!("{question} [{shown}]: "))?;
    Ok(match answer.as_str() {
        "" => current.map(str::to_owned),
        "-" => None,
        _ => Some(answer),
    })
}

fn pick_engine(title: &str, default: Kind, cfg: &Config) -> Result<Kind, String> {
    println!("\n{title}");
    for (i, kind) in ENGINES.iter().enumerate() {
        println!("  {}) {:<9} {} — {}", i + 1, kind.id(), describe(*kind), status(*kind, cfg));
    }
    loop {
        let answer = prompt(&format!("Choose 1-{} [{}]: ", ENGINES.len(), default.id()))?;
        if answer.is_empty() {
            return Ok(default);
        }
        let by_number = answer.parse::<usize>().ok().filter(|n| (1..=ENGINES.len()).contains(n)).map(|n| ENGINES[n - 1]);
        if let Some(kind) = by_number.or_else(|| ENGINES.iter().copied().find(|k| k.id() == answer)) {
            return Ok(kind);
        }
        println!("  type a number from 1 to {}, or an engine name", ENGINES.len());
    }
}

/// Engine-specific settings; returns the model (`None`: the engine's default).
async fn setup(kind: Kind, cfg: &mut Config, model: Option<String>) -> Result<Option<String>, String> {
    match kind {
        Kind::Codex | Kind::Claude => text("Model", model.as_deref(), "the engine's own default"),
        Kind::Anthropic => {
            if !env_set("ANTHROPIC_API_KEY") && !env_set("ANTHROPIC_OAUTH_TOKEN") && !anthropic_signed_in() {
                println!("  The built-in Claude agent needs a login (or ANTHROPIC_API_KEY in the environment).");
                if confirm("Sign in with your Claude subscription now?", true)? {
                    if let Err(e) = anthropic_auth::login().await {
                        println!("  sign-in failed: {e}; run `review-master --anthropic-login` later");
                    }
                }
            }
            text("Model", model.as_deref(), anthropic::DEFAULT_MODEL)
        }
        Kind::OpenAi => {
            let current = cfg.openai_base_url.clone().or_else(|| std::env::var("OPENAI_BASE_URL").ok());
            let url = loop {
                match text("Server base URL", current.as_deref(), openai::DEFAULT_BASE_URL)? {
                    Some(url) if reqwest::Url::parse(&url).is_err() => println!("  not a URL; e.g. http://localhost:8080"),
                    other => break other,
                }
            };
            cfg.openai_base_url = url.clone();
            let saved = if cfg.openai_api_key.is_some() { "Enter keeps the saved key, - removes it" } else { "Enter for none" };
            match prompt_hidden(&format!("API key, hidden ({saved}; $OPENAI_API_KEY wins when set): "))?.as_str() {
                "" => {}
                "-" => cfg.openai_api_key = None,
                key => cfg.openai_api_key = Some(key.to_owned()),
            }
            let base = url.unwrap_or_else(|| openai::DEFAULT_BASE_URL.to_owned());
            let key = std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.trim().is_empty()).or_else(|| cfg.openai_api_key.clone());
            println!("  checking {base} …");
            match openai::list_models(&base, key.as_deref()).await {
                Ok(ids) if ids.len() == 1 => {
                    println!("  the server offers one model: {}", ids[0]);
                    text("Model", model.as_deref(), "the server's only model")
                }
                Ok(ids) if !ids.is_empty() => {
                    for (i, id) in ids.iter().take(MAX_LISTED_MODELS).enumerate() {
                        println!("  {:>3}) {id}", i + 1);
                    }
                    if ids.len() > MAX_LISTED_MODELS {
                        println!("  … and {} more (type a name)", ids.len() - MAX_LISTED_MODELS);
                    }
                    loop {
                        let chosen = text("Model (number or name)", model.as_deref(), "required")?;
                        let by_number = chosen.as_deref().and_then(|c| c.parse::<usize>().ok()).and_then(|n| ids.get(n.wrapping_sub(1)).cloned());
                        match by_number.or(chosen) {
                            Some(m) => break Ok(Some(m)),
                            None => println!("  this server has several models: pick one"),
                        }
                    }
                }
                Ok(_) => {
                    println!("  the server lists no models");
                    text("Model", model.as_deref(), "required")
                }
                Err(e) => {
                    println!("  could not list models: {e}");
                    text("Model", model.as_deref(), "the server's only model")
                }
            }
        }
    }
}

/// The reasoning effort for `kind`; Enter keeps `current`, `-` returns to the default.
fn effort(kind: Kind, current: Option<String>) -> Result<Option<String>, String> {
    let (levels, default) = match kind {
        Kind::Codex => ("a level your model offers, e.g. low, medium, high", "your Codex config"),
        Kind::Claude => ("low, medium, high, xhigh or max", "Claude Code's default"),
        Kind::Anthropic => ("low, medium, high, xhigh or max; current models always think, off works only before Claude 4.6", "the API's default"),
        Kind::OpenAi => ("sent as reasoning_effort, e.g. none, low, medium, high; the server checks it", "the server's default"),
    };
    println!("  Reasoning effort: {levels}.");
    loop {
        let chosen = text("Reasoning effort", current.as_deref(), default)?;
        match (&chosen, kind) {
            (Some(e), Kind::Anthropic) if !["off", "none", "low", "medium", "high", "xhigh", "max"].contains(&e.as_str()) => {
                println!("  use low, medium, high, xhigh or max (off only on models before Claude 4.6)");
            }
            _ => return Ok(chosen),
        }
    }
}

fn summary(kind: Kind, model: Option<&str>, effort: Option<&str>, cfg: &Config) -> String {
    let model = model.map_or_else(|| "default model".to_owned(), |m| format!("model {m}"));
    let effort = effort.map_or_else(|| "default effort".to_owned(), |e| format!("effort {e}"));
    match kind {
        Kind::OpenAi => format!("{} ({model}, {effort}) at {}", kind.id(), cfg.openai_base_url.as_deref().unwrap_or(openai::DEFAULT_BASE_URL)),
        _ => format!("{} ({model}, {effort})", kind.id()),
    }
}

/// Run the wizard from `current` (if any) and save the result.
pub async fn run(current: Option<Config>) -> Result<Config, String> {
    let path = config::path()?;
    let mut cfg = current.unwrap_or_default();
    println!("review-master configuration ({})", path.display());
    println!("Enter keeps the value in [brackets], `-` clears it, Ctrl-C cancels.");

    let default_engine = cfg.engine.unwrap_or(if on_path("codex") || !on_path("claude") { Kind::Codex } else { Kind::Claude });
    let engine = pick_engine("Engine that reviews pull requests:", default_engine, &cfg)?;
    let own = cfg.engine.unwrap_or(Kind::Codex) == engine;
    let model = if own { cfg.model.clone() } else { None };
    cfg.model = setup(engine, &mut cfg, model).await?;
    cfg.effort = effort(engine, if own { cfg.effort.clone() } else { None })?;
    cfg.engine = Some(engine);

    println!("\nReviews: {}", summary(engine, cfg.model.as_deref(), cfg.effort.as_deref(), &cfg));
    if !confirm("Save?", true)? {
        return Err("configuration not saved".into());
    }
    let path = config::save(&cfg)?;
    println!("saved {} (change it with `review-master --configure`; command-line flags override it)\n", path.display());
    Ok(cfg)
}
