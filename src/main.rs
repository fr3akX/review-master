//! review-master: review a pull request (or a local worktree) in a browser UI —
//! diffs, full files with highlighted changes, line-anchored LLM chat, draft
//! comments — and submit one GitHub review with inline comments. The LLM side
//! runs like cxplr: Codex app-server, Claude Code, native Anthropic Messages or
//! any OpenAI-compatible model over a read-only checkout.

mod anthropic;
mod anthropic_auth;
mod anthropic_wire;
mod claude;
mod codex;
mod config;
mod configure;
mod diff;
mod engine;
mod lsp;
mod native;
mod openai;
mod post;
mod pr;
mod repo;
mod repo_tools;
mod review;
mod search;
mod server;
mod term;
mod update;
mod verify;

use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Review a GitHub pull request (or a local worktree) in a local web UI and
/// post the findings as line-anchored inline comments.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// What to review: a pull request (URL, <owner>/<repo>#<number>, or a bare
    /// number against the checkout's origin) or a path to a checkout/worktree,
    /// whose branch resolves to its open PR. Omitted: review the --repo
    /// worktree (its PR if one exists, else its diff against the default branch)
    target: Option<String>,
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
    /// Time limit for one agent turn (review or chat), in minutes
    #[arg(long, default_value_t = 20)]
    timeout_mins: u64,
    /// Cache directory for agent sessions and drafts (default: `.review-master`
    /// in the checkout)
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Address to serve the UI on; keep it on loopback
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 7979)]
    port: u16,
    /// Do not open the browser
    #[arg(long)]
    no_open: bool,
    /// Language server for a language, e.g. `python=pyright-langserver --stdio`;
    /// an empty command disables it. Languages: rust, python, typescript, go, c, scala
    #[arg(long = "lsp", value_name = "LANG=COMMAND", value_parser = parse_lsp)]
    lsp: Vec<(String, String)>,
    /// Disable language servers; navigation falls back to text search
    #[arg(long)]
    no_lsp: bool,
    /// Let language servers import the project build for full navigation (Metals:
    /// runs the build tool, e.g. sbt, on the repository). Off: Scala stays read-only
    #[arg(long)]
    lsp_import_build: bool,
    /// Exclude files matching this glob from the reviewed diff (repeatable)
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,
    /// Set up the global configuration interactively (engine, model, OpenAI server)
    /// and exit; also offered on the first interactive launch without one
    #[arg(long)]
    configure: bool,
    /// Install the latest release now (for installs made with the release installer) and exit
    #[arg(long)]
    update: bool,
    /// Skip the check for a newer release (also: REVIEW_MASTER_NO_UPDATE=1)
    #[arg(long)]
    no_update: bool,
}

fn parse_lsp(raw: &str) -> Result<(String, String), String> {
    let (lang, cmd) = raw.split_once('=').ok_or("expected LANG=COMMAND")?;
    let lang = lang.trim().to_ascii_lowercase();
    if !lsp::LANGUAGES.contains(&lang.as_str()) {
        return Err(format!("unknown language `{lang}`; expected one of {}", lsp::LANGUAGES.join(", ")));
    }
    Ok((lang, cmd.trim().to_owned()))
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

/// Drop `--exclude`d files from the model, in place.
fn apply_excludes(model: &mut diff::Model, exclude: &[String]) -> Result<Vec<String>, String> {
    let mut set = globset::GlobSetBuilder::new();
    for glob in exclude {
        set.add(globset::Glob::new(glob).map_err(|e| format!("--exclude {glob}: {e}"))?);
    }
    let set = set.build().map_err(|e| e.to_string())?;
    Ok(model.retain(|f| !set.is_match(&f.path)))
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// What a run reviews, however it was acquired.
struct Change {
    /// `Some` in PR mode.
    pr: Option<(pr::PrRef, pr::PrMeta)>,
    /// Directory the agent works in (worktree or the repo itself).
    root: PathBuf,
    head_sha: String,
    branch: String,
    dirty: bool,
    model: diff::Model,
    skipped: Vec<String>,
    /// Keeps a temporary worktree alive (and removes it on drop).
    _checkout: Option<pr::Checkout>,
}

fn acquire(pr_arg: Option<&str>, exclude: &[String], repo: &repo::Repo) -> Result<Change, String> {
    let origin = pr::origin_slug(&repo.root);
    // No PR given: resolve the open PR for this checkout, if any.
    let resolved = pr_arg.map(str::to_owned).or_else(|| {
        let (owner, name) = origin.as_ref()?;
        pr::resolve_pr(&repo.root, &format!("{owner}/{name}")).map(|n| n.to_string())
    });
    let Some(raw) = resolved else {
        // Local mode: the worktree's change against the default branch.
        let local = pr::local_change(&repo.root)?;
        let mut model = diff::Model::parse(&local.diff_text);
        let skipped = apply_excludes(&mut model, exclude)?;
        return Ok(Change {
            pr: None,
            root: repo.root.clone(),
            head_sha: local.head,
            branch: local.branch,
            dirty: local.dirty,
            model,
            skipped,
            _checkout: None,
        });
    };
    let pref = pr::parse_ref(&raw, origin.as_ref())?;
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
        eprintln!("note: this pull request is {state}; reviews can still be posted");
    }
    let mut model = diff::Model::parse(&pr::diff(&pref)?);
    let skipped = apply_excludes(&mut model, exclude)?;
    if !skipped.is_empty() {
        eprintln!("  excluded from the reviewed diff: {}", skipped.join(", "));
    }
    let (checkout, sha) = pr::checkout(repo, &pref, &meta.head_sha)?;
    let branch = meta.head_ref.clone();
    Ok(Change {
        pr: Some((pref, meta)),
        root: checkout.root.clone(),
        head_sha: sha,
        branch,
        dirty: false,
        model,
        skipped,
        _checkout: Some(checkout),
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.anthropic_login {
        anthropic_auth::login().await?;
        return Ok(());
    }
    if args.update {
        println!("{}", update::update_now().await?);
        return Ok(());
    }
    if !args.no_update {
        // Off the startup path; prints a one-line notice when a newer release exists.
        tokio::spawn(async {
            if let Some(message) = update::notice().await {
                println!("{message}");
            }
        });
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

    // The positional target may be a PR reference or a path to a checkout/worktree.
    let (repo_path, pr_arg) = match args.target.as_deref() {
        Some(t) if Path::new(t).is_dir() => (PathBuf::from(t), None),
        other => (args.repo.clone(), other.map(str::to_owned)),
    };
    let repo = repo::Repo::open(&repo_path)?;
    let change = acquire(pr_arg.as_deref(), &args.exclude, &repo)?;
    if change.model.files.is_empty() {
        return Err("nothing to review: the diff is empty after exclusions".into());
    }

    let cache_dir = args.cache_dir.clone().unwrap_or_else(|| repo.root.join(repo::CACHE_DIR_NAME));
    std::fs::create_dir_all(&cache_dir).map_err(|e| format!("cache {}: {e} (use --cache-dir for read-only checkouts)", cache_dir.display()))?;
    let ignore = cache_dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(ignore, "# review-master cache: local only\n*\n");
    }

    let opts = EngineOpts {
        codex: args.codex.clone(),
        claude: args.claude.clone(),
        openai_base_url: args.openai_base_url.clone().or_else(|| env("OPENAI_BASE_URL")).or(cfg.openai_base_url).unwrap_or_else(|| openai::DEFAULT_BASE_URL.to_owned()),
        openai_api_key: env("OPENAI_API_KEY").or(cfg.openai_api_key),
        timeout: Duration::from_secs(args.timeout_mins.max(1) * 60),
    };
    let engine = start_engine(choice.engine, choice.model, &opts, &change.root, &cache_dir).await?;
    let model_note = engine.model().map_or(String::new(), |m| format!(" ({m})"));
    println!("  engine {}{model_note}, checkout {}", engine.kind().id(), change.root.display());

    let draft_key = match &change.pr {
        Some((r, _)) => format!("pr-{}", r.number),
        None => change.branch.replace(['/', '\\'], "-"),
    };
    let viewer = pr::viewer();
    let lsp = lsp::Lsp::new(&change.root, !args.no_lsp, &args.lsp, args.lsp_import_build);
    let servers = lsp.configured();
    if servers.is_empty() {
        println!("  language servers: none (navigation uses text search)");
    }
    for (lang, cmd) in servers {
        println!("  language server: {lang} = {cmd}");
    }
    let state = server::AppState::new(
        change.root.clone(),
        change.pr.as_ref().map(|(r, m)| {
            (pr::PrRef { owner: r.owner.clone(), repo: r.repo.clone(), number: r.number }, clone_meta(m))
        }),
        change.head_sha.clone(),
        change.branch.clone(),
        change.dirty,
        change.model,
        change.skipped,
        engine,
        viewer,
        cache_dir.join(format!("drafts-{draft_key}.json")),
        cache_dir.join(format!("scope-{draft_key}.json")),
        cache_dir.join(format!("summary-{draft_key}.json")),
        cache_dir.join(format!("threads-{draft_key}.json")),
        lsp,
    );
    let listener = match tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("note: {}:{} is busy ({e}); using a free port", args.host, args.port);
            tokio::net::TcpListener::bind((args.host.as_str(), 0)).await?
        }
    };
    let url = format!("http://{}", listener.local_addr()?);
    println!("open {url}");
    if !args.no_open {
        open_browser(&url);
    }
    server::serve(listener, state).await?;
    drop(change._checkout); // removes the temporary worktree
    Ok(())
}

/// `PrMeta` is plain data; the server keeps its own copy.
fn clone_meta(m: &pr::PrMeta) -> pr::PrMeta {
    pr::PrMeta {
        title: m.title.clone(),
        body: m.body.clone(),
        author: m.author.clone(),
        state: m.state.clone(),
        draft: m.draft,
        merged: m.merged,
        base_ref: m.base_ref.clone(),
        head_ref: m.head_ref.clone(),
        head_sha: m.head_sha.clone(),
        url: m.url.clone(),
    }
}
