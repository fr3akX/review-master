//! Global configuration: `<config dir>/config.toml`, written by
//! `review-master --configure` (or the first interactive launch). Command-line
//! flags override it, and the usual environment variables (`OPENAI_BASE_URL`,
//! `OPENAI_API_KEY`, `OPENAI_MODEL`) override its OpenAI settings.

use crate::engine::Kind;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

const HEADER: &str = "# review-master configuration, written by `review-master --configure`.\n# Command-line flags override these settings.\n\n";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Engine that reviews pull requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<Kind>,
    /// Model for `engine`; absent: the engine's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_base_url: Option<String>,
    /// Stored in a private (0600) file; `$OPENAI_API_KEY` wins when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_api_key: Option<String>,
}

/// `$XDG_CONFIG_HOME/review-master`, else `$HOME/.config/review-master`.
pub fn dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or("cannot locate a config directory: set XDG_CONFIG_HOME or HOME")?;
    Ok(base.join("review-master"))
}

pub fn path() -> Result<PathBuf, String> {
    Ok(dir()?.join("config.toml"))
}

/// The saved configuration, or `None` when there is none yet.
pub fn load() -> Result<Option<Config>, String> {
    let path = path()?;
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map(Some).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write the configuration atomically, readable only by the user (it may hold a key).
pub fn save(config: &Config) -> Result<PathBuf, String> {
    let path = path()?;
    let dir = path.parent().expect("config path has a parent");
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let text = format!("{HEADER}{}", toml::to_string(config).map_err(|e| e.to_string())?);
    let tmp = dir.join(format!(".config.toml.{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.flush()?;
        fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Engine and model for one run.
#[derive(Debug, PartialEq)]
pub struct Choice {
    pub engine: Kind,
    pub model: Option<String>,
}

/// Combine command-line flags with the configuration. A configured model only
/// applies to the engine it was configured for, so `--engine claude` does not
/// inherit the model of a configured `openai` engine.
pub fn choose(cfg: &Config, engine: Option<Kind>, model: Option<String>) -> Choice {
    let cfg_engine = cfg.engine.unwrap_or(Kind::Codex);
    let engine = engine.unwrap_or(cfg_engine);
    let model = model.or_else(|| (engine == cfg_engine).then(|| cfg.model.clone()).flatten());
    Choice { engine, model }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn flags_override_and_models_stay_with_their_engine() {
        let c = cfg("engine = \"openai\"\nmodel = \"qwen\"\n");
        assert_eq!(choose(&c, None, None), Choice { engine: Kind::OpenAi, model: Some("qwen".into()) });
        // Another engine drops the configured model.
        assert_eq!(choose(&c, Some(Kind::Claude), None), Choice { engine: Kind::Claude, model: None });
        // Flags win.
        assert_eq!(choose(&c, None, Some("big".into())).model.as_deref(), Some("big"));
        // No configuration: Codex with its own defaults.
        assert_eq!(choose(&Config::default(), None, None), Choice { engine: Kind::Codex, model: None });
    }

    #[test]
    fn saved_files_round_trip_and_unknown_engines_are_rejected() {
        let c = Config { engine: Some(Kind::OpenAi), openai_base_url: Some("http://h:8080".into()), ..Config::default() };
        let text = toml::to_string(&c).unwrap();
        assert_eq!(text, "engine = \"openai\"\nopenai_base_url = \"http://h:8080\"\n");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
        assert!(toml::from_str::<Config>("engine = \"gpt\"\n").is_err());
    }
}
