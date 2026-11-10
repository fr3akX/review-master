//! Interactive terminal input for `--configure` and the Anthropic login: line
//! prompts, hidden input for secrets, and whether a person is at the terminal.

use std::io::{self, BufRead, IsTerminal, Write};

/// Both stdin and stdout are terminals: a person can answer prompts.
pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Print `question` and read one line, trimmed. End of input cancels.
pub fn prompt(question: &str) -> Result<String, String> {
    print!("{question}");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
    if read == 0 {
        return Err("cancelled (end of input)".into());
    }
    Ok(line.trim().to_owned())
}

/// Like [`prompt`], with the typed or pasted text hidden.
pub fn prompt_hidden(question: &str) -> Result<String, String> {
    let _hidden = EchoGuard::hide();
    prompt(question)
}

/// Hides typed/pasted input on an interactive terminal; restores on drop.
pub struct EchoGuard {
    #[cfg(unix)]
    saved: Option<libc::termios>,
}

impl EchoGuard {
    pub fn hide() -> Self {
        Self {
            #[cfg(unix)]
            saved: hide_echo(),
        }
    }
}

/// Turns off echo (keeping the newline echo) when stdin is a terminal; returns
/// the attributes to restore.
#[cfg(unix)]
fn hide_echo() -> Option<libc::termios> {
    // SAFETY: termios calls on stdin with a zeroed, then kernel-filled struct.
    unsafe {
        if libc::isatty(libc::STDIN_FILENO) != 1 {
            return None;
        }
        let mut term: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut term) != 0 {
            return None;
        }
        let saved = term;
        term.c_lflag &= !libc::ECHO;
        term.c_lflag |= libc::ECHONL;
        (libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) == 0).then_some(saved)
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(saved) = self.saved {
            // SAFETY: restores the attributes read in `hide`.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved);
            }
        }
    }
}
