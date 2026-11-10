//! Client for Claude Code in headless mode (`claude -p`), one process per turn.
//!
//! Sessions are Claude Code's own persisted conversations
//! (`~/.claude/projects/<project>/<id>.jsonl`; `claude --resume <id>` opens one).
//! review-master picks every session id: a new session's first turn creates it with
//! `--session-id`, a fork's first turn resumes the parent with `--fork-session`
//! under the new id, and later turns resume it. Turns run in `--restricted` mode
//! with only the Read, Grep and Glob tools: nothing that runs commands, no MCP
//! servers, and no hooks or other settings from the user's or the repository's
//! settings files.

use crate::engine::{one_line, Outcome, SESSION_GONE};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, fs};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;

const TOOLS: &str = "Read,Grep,Glob";
const STDERR_TAIL: usize = 8 * 1024;

const SYSTEM_PROMPT: &str = "\
You are review-master's read-only pull-request review agent. The pull request's checkout is your working \
directory. Explore it with the Read, Grep and Glob tools; Read shows line numbers and \
Grep shows them with `-n`, so never estimate a line number. Cite paths relative to the \
repository root. When a turn defines an output schema, answer with exactly that JSON.";

pub struct ClaudeConfig {
    pub bin: String,
    pub model: Option<String>,
    /// Limit for one turn.
    pub timeout: Duration,
}

/// A session handed out whose first turn has not run yet.
struct Unborn {
    /// Session to fork on the first turn.
    parent: Option<String>,
    name: String,
}

pub struct Claude {
    cfg: ClaudeConfig,
    cwd: PathBuf,
    /// Claude Code's transcript directory: `<config dir>/projects`.
    projects: PathBuf,
    unborn: Mutex<HashMap<String, Unborn>>,
    /// Sessions with a turn running.
    busy: Mutex<HashSet<String>>,
}

/// Marks a session idle again however its turn ends.
struct Busy<'a> {
    claude: &'a Claude,
    session: String,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.claude.busy.lock().unwrap().remove(&self.session);
    }
}

impl Claude {
    /// Check that `claude` exists and is new enough; nothing runs until the first turn.
    pub async fn start(cfg: ClaudeConfig, cwd: &Path) -> Result<Claude, String> {
        let help = Command::new(&cfg.bin)
            .arg("--help")
            .output()
            .await
            .map_err(|e| format!("cannot run `{}`: {e} (install Claude Code, or point --claude at it)", cfg.bin))?;
        let help = String::from_utf8_lossy(&help.stdout);
        for flag in ["--restricted", "--fork-session", "--session-id", "--json-schema"] {
            if !help.contains(flag) {
                return Err(format!("`{}` has no {flag} option; review-master needs a newer Claude Code (`claude update`)", cfg.bin));
            }
        }
        let config = env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))
            .ok_or("neither CLAUDE_CONFIG_DIR nor HOME is set")?;
        Ok(Claude::new(cfg, cwd, config.join("projects")))
    }

    fn new(cfg: ClaudeConfig, cwd: &Path, projects: PathBuf) -> Claude {
        Claude { cfg, cwd: cwd.to_owned(), projects, unborn: Mutex::default(), busy: Mutex::default() }
    }

    pub fn model(&self) -> Option<&str> {
        self.cfg.model.as_deref()
    }

    pub fn start_session(&self, name: &str) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.unborn.lock().unwrap().insert(id.clone(), Unborn { parent: None, name: name.to_owned() });
        Ok(id)
    }

    pub fn fork(&self, parent: &str, name: &str) -> Result<String, String> {
        if self.transcript(parent).is_none() {
            return Err(format!("{SESSION_GONE}: {parent}"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.unborn.lock().unwrap().insert(id.clone(), Unborn { parent: Some(parent.to_owned()), name: name.to_owned() });
        Ok(id)
    }

    /// The transcript of a session that has run. Claude Code derives the project
    /// directory from the working directory; ids are unique, so any project will do.
    fn transcript(&self, id: &str) -> Option<PathBuf> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return None;
        }
        let file = format!("{id}.jsonl");
        fs::read_dir(&self.projects).ok()?.filter_map(Result::ok).map(|e| e.path().join(&file)).find(|p| p.is_file())
    }

    pub fn delete_session(&self, id: &str) -> Result<(), String> {
        self.unborn.lock().unwrap().remove(id);
        let Some(file) = self.transcript(id) else { return Ok(()) };
        fs::remove_file(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        // Tool results and subagent transcripts, when the session has any.
        let dir = file.with_extension("");
        if dir.is_dir() {
            let _ = fs::remove_dir_all(dir);
        }
        Ok(())
    }

    /// How a turn reaches `session`: resume it, or create it on its first turn.
    fn session_args(&self, session: &str) -> Result<Vec<String>, String> {
        if self.transcript(session).is_some() {
            return Ok(vec!["--resume".into(), session.into()]);
        }
        let unborn = self.unborn.lock().unwrap();
        let first = unborn.get(session).ok_or_else(|| format!("{SESSION_GONE}: {session}"))?;
        let mut args = match &first.parent {
            Some(parent) => vec!["--resume".to_owned(), parent.clone(), "--fork-session".into()],
            None => vec![],
        };
        args.extend(["--session-id".into(), session.into(), "--name".into(), first.name.clone()]);
        Ok(args)
    }

    /// Run one turn in `session`; the answer is Claude Code's structured output.
    pub async fn turn(
        &self,
        session: &str,
        prompt: &str,
        schema: &Value,
        cancel: &Notify,
        mut progress: impl FnMut(String),
    ) -> Result<Outcome<Value>, String> {
        if !self.busy.lock().unwrap().insert(session.to_owned()) {
            return Err("this session is busy with another turn".into());
        }
        let _busy = Busy { claude: self, session: session.to_owned() };

        let mut cmd = Command::new(&self.cfg.bin);
        cmd.args(["-p", "--output-format", "stream-json", "--verbose"])
            .args(["--restricted", "--strict-mcp-config", "--disable-slash-commands"])
            .args(["--tools", TOOLS, "--allowedTools", TOOLS])
            .args(["--permission-mode", "dontAsk", "--permission-prompts", "none"])
            .args(["--append-system-prompt", SYSTEM_PROMPT])
            .arg("--json-schema")
            .arg(schema.to_string());
        if let Some(model) = &self.cfg.model {
            cmd.args(["--model", model]);
        }
        cmd.args(self.session_args(session)?);
        let mut child = cmd
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start `{}`: {e}", self.cfg.bin))?;

        // The prompt goes through stdin: a map's prompt can exceed the size of one argument.
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(prompt.as_bytes()).await.map_err(|e| format!("claude: {e}"))?;
        drop(stdin);
        let tail = Arc::new(Mutex::new(String::new()));
        let mut stderr = child.stderr.take().expect("piped stderr");
        let tail_w = tail.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut chunk).await {
                let mut t = tail_w.lock().unwrap();
                t.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if t.len() > STDERR_TAIL {
                    let mut cut = t.len() - STDERR_TAIL;
                    while !t.is_char_boundary(cut) {
                        cut += 1;
                    }
                    t.drain(..cut);
                }
            }
        });

        let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        let mut state = TurnState::default();
        let deadline = tokio::time::sleep(self.cfg.timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) => state.handle(&line, &self.cwd, &mut progress),
                    _ => break,
                },
                _ = cancel.notified() => {
                    let _ = child.kill().await;
                    return Err("cancelled".into());
                }
                _ = &mut deadline => {
                    let _ = child.kill().await;
                    return Err(format!("timed out after {} minutes", self.cfg.timeout.as_secs() / 60));
                }
            }
        }
        let status = child.wait().await.map_err(|e| format!("claude: {e}"))?;
        let Some(result) = state.result else {
            let t = tail.lock().unwrap().clone();
            let why = t.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output").trim().to_owned();
            return Err(format!("claude exited ({status}) without a result: {why}"));
        };
        if result["is_error"] == true || result["subtype"] != "success" {
            let errors: Vec<&str> = result["errors"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let why = if errors.is_empty() {
                result["result"].as_str().filter(|r| !r.is_empty()).or(result["subtype"].as_str()).unwrap_or("unknown error").to_owned()
            } else {
                errors.join("; ")
            };
            return Err(format!("claude turn failed: {}", one_line(&why, 400)));
        }
        self.unborn.lock().unwrap().remove(session);
        let answer = result.get("structured_output").filter(|v| !v.is_null()).cloned().ok_or_else(|| {
            format!("claude finished the turn without a structured answer: {}", one_line(result["result"].as_str().unwrap_or(""), 400))
        })?;
        let usage = &result["usage"];
        let tokens = |key: &str| usage[key].as_u64().unwrap_or(0);
        Ok(Outcome {
            answer,
            commands: state.commands,
            input_tokens: tokens("input_tokens") + tokens("cache_read_input_tokens") + tokens("cache_creation_input_tokens"),
            cached_input_tokens: tokens("cache_read_input_tokens"),
            output_tokens: tokens("output_tokens"),
        })
    }
}

#[derive(Default)]
struct TurnState {
    result: Option<Value>,
    commands: u32,
}

impl TurnState {
    fn handle(&mut self, line: &str, cwd: &Path, progress: &mut impl FnMut(String)) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { return };
        match msg["type"].as_str().unwrap_or_default() {
            "assistant" => {
                for block in msg["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str().unwrap_or_default() {
                        // The schema answer arrives as a tool call; it is not exploration.
                        "tool_use" if block["name"] != "StructuredOutput" => {
                            self.commands += 1;
                            progress(format!("$ {}", describe(block, cwd)));
                        }
                        "text" => {
                            let text = block["text"].as_str().unwrap_or_default().trim();
                            if !text.is_empty() && !text.starts_with('{') {
                                progress(format!("› {}", one_line(text, 300)));
                            }
                        }
                        "thinking" => {
                            let text = block["thinking"].as_str().unwrap_or_default();
                            if !text.trim().is_empty() {
                                progress(format!("… {}", one_line(text, 300)));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "system" if msg["subtype"] == "compact_boundary" => progress("… Claude Code compacted the session context".into()),
            "result" => self.result = Some(msg),
            _ => {}
        }
    }
}

/// `Read src/main.rs (lines 40+80)`, `Grep "fn main" src`, `Glob **/*.toml`.
fn describe(block: &Value, cwd: &Path) -> String {
    let input = &block["input"];
    let rel = |key: &str| {
        let path = input[key].as_str().unwrap_or_default();
        Path::new(path).strip_prefix(cwd).map_or(path.to_owned(), |p| p.to_string_lossy().into_owned())
    };
    let name = block["name"].as_str().unwrap_or("tool");
    match name {
        "Read" => {
            let mut s = format!("Read {}", rel("file_path"));
            if let Some(offset) = input["offset"].as_u64() {
                s.push_str(&format!(" (lines {offset}+{})", input["limit"].as_u64().map_or("…".into(), |l| l.to_string())));
            }
            s
        }
        "Grep" => {
            let mut s = format!("Grep {:?}", input["pattern"].as_str().unwrap_or_default());
            if input["path"].is_string() {
                s.push_str(&format!(" {}", rel("path")));
            }
            if let Some(glob) = input["glob"].as_str() {
                s.push_str(&format!(" --glob {glob}"));
            }
            s
        }
        "Glob" => {
            let mut s = format!("Glob {}", input["pattern"].as_str().unwrap_or_default());
            if input["path"].is_string() {
                s.push_str(&format!(" in {}", rel("path")));
            }
            s
        }
        _ => format!("{name} {}", one_line(&input.to_string(), 120)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_created_forked_or_resumed_on_their_turn() {
        let dir = std::env::temp_dir().join(format!("review-master-claude-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("projects/-repo")).unwrap();
        let cfg = ClaudeConfig { bin: "claude".into(), model: None, timeout: Duration::from_secs(60) };
        let claude = Claude::new(cfg, Path::new("/repo"), dir.join("projects"));

        let parent = claude.start_session("system").unwrap();
        assert_eq!(claude.session_args(&parent).unwrap(), ["--session-id", &parent, "--name", "system"]);
        // Only a session that has run can be forked.
        assert!(claude.fork(&parent, "child").unwrap_err().starts_with(SESSION_GONE));

        fs::write(dir.join(format!("projects/-repo/{parent}.jsonl")), "{}\n").unwrap();
        assert_eq!(claude.session_args(&parent).unwrap(), ["--resume", &parent]);
        let child = claude.fork(&parent, "child").unwrap();
        assert_eq!(
            claude.session_args(&child).unwrap(),
            ["--resume", &parent, "--fork-session", "--session-id", &child, "--name", "child"]
        );

        // Deleted (or never known to this process) sessions are reported as gone.
        claude.delete_session(&parent).unwrap();
        assert!(claude.session_args(&parent).unwrap_err().starts_with(SESSION_GONE));
        assert!(claude.session_args("../../etc/passwd").unwrap_err().starts_with(SESSION_GONE));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn turns_count_exploration_tools_but_not_the_structured_answer() {
        let cwd = Path::new("/repo");
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"s"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/repo/src/main.rs","offset":10,"limit":40}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Checking the router."},{"type":"tool_use","name":"Grep","input":{"pattern":"fn route","path":"/repo/src"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"StructuredOutput","input":{"value":42}}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"structured_output":{"value":42}}"#,
        ];
        let mut state = TurnState::default();
        let mut log = Vec::new();
        for line in lines {
            state.handle(line, cwd, &mut |l| log.push(l));
        }
        assert_eq!(state.commands, 2);
        assert_eq!(log, ["$ Read src/main.rs (lines 10+40)", "› Checking the router.", "$ Grep \"fn route\" src"]);
        assert_eq!(state.result.unwrap()["structured_output"]["value"], 42);
    }
}
