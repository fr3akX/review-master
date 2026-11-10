//! review-master: agent pull-request reviews posted as line-anchored inline
//! comments. The review runs like cxplr's explorations — Codex app-server,
//! Claude Code, native Anthropic Messages or any OpenAI-compatible model over a
//! read-only checkout — but the subject is a PR head and the answer is one
//! GitHub review whose findings sit on the exact diff lines.

mod anthropic;
mod anthropic_auth;
mod anthropic_wire;
mod claude;
mod codex;
mod config;
mod configure;
mod diff;
mod engine;
mod native;
mod openai;
mod post;
mod pr;
mod repo;
mod repo_tools;
mod review;
mod term;
mod verify;

use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Review a GitHub pull request with an agent and post the findings as
/// line-anchored inline comments.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Pull request: URL, <owner>/<repo>#<number>, or a bare number
    /// (resolved against the checkout's GitHub origin)
    pr: Option<String>,
    /// Local checkout of the repository; the PR head is fetched into a
    /// temporary worktree when the checkout is not already on it
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Agent: codex app-server, claude -p, native Anthropic Messages, or an
    /// OpenAI-compatible Chat Completions server (default: the configured engine,
    /// else codex)
    #[arg(long, value_enum)]
    engine: Option<engine::Kind>,
    /// Model (default: the configured model; CLI engines use their own config,
    /// native Anthropic claude-sonnet-5-5, openai $OPENAI_MODEL or the server's only model)
    #[arg(long)]
    model: Option<String>,
    /// OpenAI-compatible API root for --engine openai (also $OPENAI_BASE_URL; a bare
    /// host gets /v1). The API key, if the server needs one, comes from $OPENAI_API_KEY
    #[arg(long)]
    openai_base_url: Option<String>,
    /// Codex executable (run as `codex app-server`)
    #[arg(long, default_value = "codex")]
    codex: String,
    /// Claude Code executable (run as `claude -p`)
    #[arg(long, default_value = "claude")]
    claude: String,
    /// Sign into native Anthropic with Claude subscription OAuth and exit
    #[arg(long)]
    anthropic_login: bool,
    /// Time limit for the review, in minutes
    #[arg(long, default_value_t = 20)]
    timeout_mins: u64,
    /// Cache directory for agent sessions (default: `.review-master` in the checkout)
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Review event to submit; auto follows the model's verdict
    #[arg(long, value_enum, default_value_t = EventArg::Auto)]
    event: EventArg,
    /// Print the review and the exact API payload instead of posting
    #[arg(long)]
    dry_run: bool,
    /// Exclude files matching this glob from the reviewed diff (repeatable)
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,
    /// Set up the global configuration interactively (engine, model, OpenAI server)
    /// and exit; also offered on the first interactive launch without one
    #[arg(long)]
    configure: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum EventArg {
    Auto,
    Comment,
    Approve,
    RequestChanges,
}

/// Engine settings resolved from flags, the environment and the configuration.
struct EngineOpts {
    codex: String,
    claude: String,
    openai_base_url: String,
    openai_api_key: Option<String>,
    timeout: Duration,
}

async fn start_engine(kind: engine::Kind, model: Option<String>, o: &EngineOpts, root: &Path, cache_dir: &Path) -> Result<engine::Engine, String> {
    let timeout = o.timeout;
    Ok(match kind {
        engine::Kind::Codex => {
            let cfg = codex::CodexConfig { bin: o.codex.clone(), model, timeout };
            engine::Engine::Codex(codex::Codex::start(cfg, root).await?)
        }
        engine::Kind::Claude => {
            let cfg = claude::ClaudeConfig { bin: o.claude.clone(), model, timeout };
            engine::Engine::Claude(Arc::new(claude::Claude::start(cfg, root).await?))
        }
        engine::Kind::Anthropic => {
            let model = model.or_else(|| Some(anthropic::DEFAULT_MODEL.to_owned()));
            engine::Engine::Anthropic(Arc::new(anthropic::Anthropic::start(root, cache_dir, model, timeout).await?))
        }
        engine::Kind::OpenAi => {
            let cfg = openai::OpenAiConfig { base_url: o.openai_base_url.clone(), api_key: o.openai_api_key.clone(), model, timeout };
            engine::Engine::OpenAi(Arc::new(openai::OpenAi::start(root, cache_dir, cfg).await?))
        }
    })
}

/// Drop `--exclude`d files from the diff; then, over the budget, defer whole
/// files to the excluded note rather than cutting a file mid-hunk.
fn filter_diff(text: String, exclude: &[String]) -> Result<(String, Vec<String>), String> {
    let mut set = globset::GlobSetBuilder::new();
    for glob in exclude {
        set.add(globset::Glob::new(glob).map_err(|e| format!("--exclude {glob}: {e}"))?);
    }
    let set = set.build().map_err(|e| e.to_string())?;
    if exclude.is_empty() && text.len() <= review::MAX_DIFF_BYTES {
        return Ok((text, Vec::new()));
    }
    let mut kept = String::new();
    let mut skipped = Vec::new();
    for (path, segment) in diff::split_files(&text) {
        if set.is_match(&path) || kept.len() + segment.len() > review::MAX_DIFF_BYTES {
            skipped.push(path);
        } else {
            kept.push_str(&segment);
        }
    }
    Ok((kept, skipped))
}

fn location(path: &str, start: Option<u64>, line: u64) -> String {
    match start {
        Some(start) => format!("{path}:{start}-{line}"),
        None => format!("{path}:{line}"),
    }
}

fn headline(f: &review::Finding) -> String {
    engine::one_line(f.title.as_deref().unwrap_or(&f.body), 80)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.anthropic_login {
        anthropic_auth::login().await?;
        return Ok(());
    }
    if args.configure {
        if !term::interactive() {
            return Err("--configure asks questions: run it in a terminal".into());
        }
        configure::run(config::load()?).await?;
        return Ok(());
    }
    let mut cfg = config::load()?;
    // First interactive launch without a configuration: set one up, then carry on.
    let engine_flags = args.engine.is_some() || args.model.is_some() || args.openai_base_url.is_some();
    if cfg.is_none() && !engine_flags && term::interactive() {
        println!("No review-master configuration yet; let's set it up first (`review-master --configure` changes it later).\n");
        cfg = Some(configure::run(None).await?);
    }
    let cfg = cfg.unwrap_or_default();
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let mut choice = config::choose(&cfg, args.engine, args.model.clone());
    // $OPENAI_MODEL sits between flags and the configuration.
    if choice.engine == engine::Kind::OpenAi && args.model.is_none() {
        choice.model = env("OPENAI_MODEL").or(choice.model);
    }
    let Some(raw) = args.pr.as_deref() else {
        return Err("give a pull request to review: a URL, <owner>/<repo>#<number>, or a number".into());
    };

    let repo = repo::Repo::open(&args.repo)?;
    let origin = pr::origin_slug(&repo.root);
    let pref = pr::parse_ref(raw, origin.as_ref())?;
    if let Some((owner, name)) = &origin {
        if !owner.eq_ignore_ascii_case(&pref.owner) || !name.eq_ignore_ascii_case(&pref.repo) {
            eprintln!("note: the checkout's origin is {owner}/{name}, not {}; fetching the PR head may fail", pref.slug());
        }
    }
    let meta = pr::meta(&pref)?;
    let mut state = if meta.merged { "merged".to_owned() } else { meta.state.clone() };
    if meta.draft {
        state.push_str(", draft");
    }
    println!("review-master: {}#{} — {}", pref.slug(), pref.number, meta.title);
    println!("  {} wants `{}` ← `{}`; {state}; head {}", meta.author, meta.base_ref, meta.head_ref, &meta.head_sha[..meta.head_sha.len().min(12)]);
    println!("  {}", meta.url);
    if meta.state != "open" {
        eprintln!("note: this pull request is {state}; the review will still be posted");
    }

    let (kept, skipped) = filter_diff(pr::diff(&pref)?, &args.exclude)?;
    if !skipped.is_empty() {
        eprintln!("  excluded from the reviewed diff: {}", skipped.join(", "));
    }
    let dmap = diff::Diff::parse(&kept);
    if dmap.is_empty() {
        return Err("nothing to review: the diff is empty after exclusions".into());
    }

    let (checkout, sha) = pr::checkout(&repo, &pref, &meta.head_sha)?;
    let cache_dir = args.cache_dir.clone().unwrap_or_else(|| repo.root.join(repo::CACHE_DIR_NAME));
    std::fs::create_dir_all(&cache_dir).map_err(|e| format!("cache {}: {e} (use --cache-dir for read-only checkouts)", cache_dir.display()))?;
    let ignore = cache_dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(ignore, "# review-master cache: local only\n*\n");
    }

    let opts = EngineOpts {
        codex: args.codex,
        claude: args.claude,
        openai_base_url: args.openai_base_url.or_else(|| env("OPENAI_BASE_URL")).or(cfg.openai_base_url).unwrap_or_else(|| openai::DEFAULT_BASE_URL.to_owned()),
        openai_api_key: env("OPENAI_API_KEY").or(cfg.openai_api_key),
        timeout: Duration::from_secs(args.timeout_mins.max(1) * 60),
    };
    let engine = start_engine(choice.engine, choice.model, &opts, &checkout.root, &cache_dir).await?;
    let model = engine.model().map_or(String::new(), |m| format!(" ({m})"));
    println!("  engine {}{model}, checkout {}", engine.kind().id(), checkout.root.display());

    let cancel = Arc::new(tokio::sync::Notify::new());
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\ncancelling…");
                cancel.notify_waiters();
            }
        });
    }
    let prompt = review::prompt(&pref, &meta, &sha, &kept, &skipped);
    let name = format!("review-master: {}#{}", pref.slug(), pref.number);
    let reviewed = review::run(&engine, &name, &prompt, &dmap, &cancel, |m| println!("  {m}")).await?;

    let event = match args.event {
        EventArg::Auto => post::Event::from_verdict(&reviewed.verdict),
        EventArg::Comment => post::Event::Comment,
        EventArg::Approve => post::Event::Approve,
        EventArg::RequestChanges => post::Event::RequestChanges,
    };
    println!("\nverdict {} → {}; {} inline, {} in the review body", reviewed.verdict, event.api(), reviewed.inline.len(), reviewed.fallback.len());
    for a in &reviewed.inline {
        println!("  inline  {}  [{}] {}", location(&a.finding.path, a.start, a.line), a.finding.severity, headline(&a.finding));
    }
    for (f, why) in &reviewed.fallback {
        println!("  body    {}  [{}] {} ({why})", f.location(), f.severity, headline(f));
    }
    println!("  {} tool calls, {} in / {} cached / {} out tokens", reviewed.commands, reviewed.input_tokens, reviewed.cached_input_tokens, reviewed.output_tokens);

    let payload = post::payload(&sha, event, &reviewed.summary, &reviewed.inline, &reviewed.fallback);
    if args.dry_run {
        println!("\ndry run — would POST repos/{}/pulls/{}/reviews:", pref.slug(), pref.number);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        let url = post::submit(&pref, &payload)?;
        println!("\nposted {url}");
    }
    drop(checkout); // removes the temporary worktree
    drop(engine); // stops codex app-server before exiting
    Ok(())
}
