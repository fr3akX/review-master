//! Engine sign-in driven from the web UI. Each engine's own login command runs as a
//! child process — `codex login`, `claude auth login`, `review-master --anthropic-login`
//! — whose output (with the sign-in link) the UI shows. A code the provider shows
//! instead of redirecting back can be pasted; it goes to the command's stdin. Nothing
//! here handles a credential: the commands store their own.

use crate::engine::{EngineOpts, Kind};
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::Notify;

const MAX_LINES: usize = 400;

/// The sign-in command for an engine; `None` when it has none to run.
pub fn command_for(kind: Kind, opts: &EngineOpts) -> Option<(String, Vec<String>)> {
    match kind {
        Kind::Codex => Some((opts.codex.clone(), vec!["login".into()])),
        Kind::Claude => Some((opts.claude.clone(), vec!["auth".into(), "login".into()])),
        Kind::Anthropic => {
            let exe = std::env::current_exe().ok()?;
            Some((exe.to_string_lossy().into_owned(), vec!["--anthropic-login".into()]))
        }
        Kind::OpenAi => None,
    }
}

/// One running (or finished) sign-in.
pub struct LoginRun {
    pub kind: Kind,
    lines: Mutex<Vec<String>>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    /// `Some` once the command exited: `Ok` on success, else why it failed.
    outcome: Mutex<Option<Result<(), String>>>,
    cancel: Notify,
}

/// Terminal colour and cursor sequences, which some CLIs print even into a pipe.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if c != '\r' {
            out.push(c);
        }
    }
    out
}

/// `http(s)://…` links in the output, in order, without duplicates.
fn links(lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        for (i, _) in line.match_indices("http") {
            let rest = &line[i..];
            if !(rest.starts_with("http://") || rest.starts_with("https://")) {
                continue;
            }
            let end = rest.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')')).unwrap_or(rest.len());
            let url = rest[..end].trim_end_matches(['.', ',']).to_owned();
            if !out.contains(&url) {
                out.push(url);
            }
        }
    }
    out
}

impl LoginRun {
    /// Start `program args`; the run collects its output until it exits or is cancelled.
    pub fn start(kind: Kind, program: &str, args: &[String]) -> Result<Arc<LoginRun>, String> {
        let mut child = Command::new(program)
            .args(args)
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot run `{program}`: {e}"))?;
        let run = Arc::new(LoginRun {
            kind,
            lines: Mutex::new(Vec::new()),
            stdin: tokio::sync::Mutex::new(child.stdin.take()),
            outcome: Mutex::new(None),
            cancel: Notify::new(),
        });
        let readers: Vec<_> = [child.stdout.take().map(|o| Box::new(o) as Box<dyn tokio::io::AsyncRead + Send + Unpin>), child.stderr.take().map(|e| Box::new(e) as _)]
            .into_iter()
            .flatten()
            .map(|stream| {
                let run = run.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stream).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let line = strip_ansi(&line);
                        let mut all = run.lines.lock().unwrap();
                        if all.len() < MAX_LINES && !line.trim().is_empty() {
                            all.push(line);
                        }
                    }
                })
            })
            .collect();
        let waiter = run.clone();
        let program = program.to_owned();
        tokio::spawn(async move {
            let outcome = tokio::select! {
                status = child.wait() => match status {
                    Ok(s) if s.success() => Ok(()),
                    Ok(s) => Err(format!("`{program}` exited with {s}")),
                    Err(e) => Err(format!("`{program}`: {e}")),
                },
                _ = waiter.cancel.notified() => {
                    let _ = child.kill().await;
                    Err("cancelled".to_owned())
                }
            };
            // Let the readers drain what the command printed last.
            for reader in readers {
                let _ = reader.await;
            }
            *waiter.outcome.lock().unwrap() = Some(outcome);
        });
        Ok(run)
    }

    /// Send a line (a pasted code) to the command.
    pub async fn send(&self, text: &str) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        let pipe = stdin.as_mut().ok_or("the sign-in takes no input")?;
        pipe.write_all(format!("{}\n", text.trim()).as_bytes()).await.map_err(|e| format!("cannot send the code: {e}"))?;
        pipe.flush().await.map_err(|e| format!("cannot send the code: {e}"))
    }

    pub fn cancel(&self) {
        self.cancel.notify_one();
    }

    pub fn running(&self) -> bool {
        self.outcome.lock().unwrap().is_none()
    }

    /// What the UI shows: output, links found in it, and how it ended.
    pub fn json(&self) -> Value {
        let lines = self.lines.lock().unwrap().clone();
        let outcome = self.outcome.lock().unwrap().clone();
        json!({
            "kind": self.kind.id(),
            "links": links(&lines),
            "lines": lines,
            "running": outcome.is_none(),
            "ok": outcome.as_ref().map(Result::is_ok),
            "error": outcome.and_then(Result::err),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_cleaned_and_links_found() {
        assert_eq!(strip_ansi("\u{1b}[1;32mOpen\u{1b}[0m this\r"), "Open this");
        let lines = vec![
            "Go to https://auth.example.com/authorize?x=1&y=2.".to_owned(),
            "or (http://localhost:1455/cb) — https://auth.example.com/authorize?x=1&y=2".to_owned(),
        ];
        assert_eq!(links(&lines), ["https://auth.example.com/authorize?x=1&y=2", "http://localhost:1455/cb"]);
    }

    #[tokio::test]
    async fn a_run_shows_output_takes_a_pasted_code_and_reports_how_it_ended() {
        let script = "echo 'Visit https://example.com/login'; read code; echo \"got $code\"; [ \"$code\" = abc ]";
        let run = LoginRun::start(Kind::Codex, "sh", &["-c".into(), script.into()]).unwrap();
        for _ in 0..50 {
            if !run.lines.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(run.json()["links"], json!(["https://example.com/login"]));
        assert!(run.running());
        run.send(" abc ").await.unwrap();
        for _ in 0..100 {
            if !run.running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let state = run.json();
        assert_eq!((state["ok"].as_bool(), state["running"].as_bool()), (Some(true), Some(false)));
        assert_eq!(state["lines"][1], "got abc");

        let stuck = LoginRun::start(Kind::Claude, "sh", &["-c".into(), "sleep 30".into()]).unwrap();
        stuck.cancel();
        for _ in 0..100 {
            if !stuck.running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(stuck.json()["error"], "cancelled");
    }
}
