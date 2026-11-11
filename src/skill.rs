//! The review-master skill for Codex and Claude Code: when such a session is asked to
//! review a change, it writes the review in the Markdown shape review-master imports
//! (`--findings`), so the findings land on the cited lines of the review UI. Both agents
//! read `skills/<name>/SKILL.md` with the same frontmatter from their config directory.

use std::path::PathBuf;
use std::{env, fs, io};

pub const SKILL: &str = r#"---
name: review-master
description: Use when asked to review a pull request or a branch's changes for the user to read in review-master, or when the user mentions review-master or `--findings`. Write the review as a Markdown file of findings that cite `path:line`; review-master shows each one on that line, where the user turns it into a draft comment or dismisses it.
---

# Reviews for review-master

review-master is the user's local code-review UI. It imports a Markdown review and
shows every finding on the line it cites, with its severity. The user decides what
happens to each: turn it into a draft comment (which they edit), ask about it, or
dismiss it. Nothing you write reaches GitHub unless the user saves it as a draft and
submits the review themselves — so write for a reviewer who will judge each finding.

## Write the review

1. Read the change: `gh pr diff <N>` (or `git diff <base>...HEAD` for a branch), and
   the surrounding code where the diff alone does not settle a question. Do not
   modify the repository.
2. One finding per issue, each a heading with severity, title and location:

   ```markdown
   ### [major] Hit counter is not thread-safe (src/store/cache.rs:17)

   `hits` is a plain `u64` bumped through `&mut self`, so every reader now needs
   exclusive access. Use an `AtomicU64` and keep `get(&self)`.
   ```

   - severity: `blocker`, `major`, `minor` or `nit`.
   - location: repository-relative path and line at the PR head, or a range
     `path:12-18` covering the lines the issue is about. Take numbers from the
     new-file side of the diff hunks or a numbered read of the head file; never
     estimate. Prefer lines the diff adds or shows: a finding there can become an
     inline comment, one elsewhere goes into the review body.
   - body: what is wrong, why it matters, and a concrete fix. Markdown; further
     `path:line` references become clickable.
3. Only findings. Sections without a `path:line` are not imported; put an overall
   verdict, if any, under `## Summary` (review-master leaves overview sections out).
4. Save it as `review-<N>.md` outside the repository (e.g. in `/tmp`), or where the
   user asks. When you revise a review, keep the file name: importing it again
   replaces the earlier findings and keeps the ones the user dismissed.

## Open it in review-master

Tell the user the command, run from the repository checkout:

```sh
review-master <N> --findings /tmp/review-<N>.md
```

(`<N>` may also be a PR URL or `owner/repo#N`; without a PR, review-master reviews the
current branch against the default branch.) Into a running review-master, the file can
be imported from the Findings drawer (`f`) or dropped on the page. If the user asks you
to start it, run it in the background: it serves the UI until stopped.
"#;

/// Agents the skill is written for, by their config directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Agent {
    Codex,
    Claude,
}

/// What `--install-skill` installs for: one agent, or every agent whose config directory exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Target {
    All,
    Codex,
    Claude,
}

impl Agent {
    const ALL: [Agent; 2] = [Agent::Codex, Agent::Claude];

    /// `$CODEX_HOME` (default `~/.codex`) or `$CLAUDE_CONFIG_DIR` (default `~/.claude`).
    fn config_dir(self) -> io::Result<PathBuf> {
        let (var, default) = match self {
            Agent::Codex => ("CODEX_HOME", ".codex"),
            Agent::Claude => ("CLAUDE_CONFIG_DIR", ".claude"),
        };
        env::var_os(var)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(default)))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("neither {var} nor HOME is set")))
    }

    fn skill_path(self) -> io::Result<PathBuf> {
        Ok(self.config_dir()?.join("skills").join("review-master").join("SKILL.md"))
    }
}

/// Write the skill to `<agent config dir>/skills/review-master/SKILL.md`. Returns the written paths.
pub fn install(target: Target) -> Result<Vec<PathBuf>, String> {
    let agents: Vec<Agent> = match target {
        Target::Codex => vec![Agent::Codex],
        Target::Claude => vec![Agent::Claude],
        Target::All => Agent::ALL.into_iter().filter(|a| a.config_dir().is_ok_and(|d| d.is_dir())).collect(),
    };
    if agents.is_empty() {
        return Err("found neither ~/.codex nor ~/.claude: name the agent, e.g. `--install-skill claude`".into());
    }
    agents
        .into_iter()
        .map(|a| {
            let path = a.skill_path().map_err(|e| e.to_string())?;
            fs::create_dir_all(path.parent().expect("skill path has a parent")).map_err(|e| format!("{}: {e}", path.display()))?;
            fs::write(&path, SKILL).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(path)
        })
        .collect()
}

/// Keep installed skills in step with this version of review-master (e.g. after an
/// update). Returns the paths that were rewritten.
pub fn refresh_installed() -> Vec<PathBuf> {
    Agent::ALL
        .into_iter()
        .filter_map(|a| {
            let path = a.skill_path().ok()?;
            let current = fs::read_to_string(&path).ok()?;
            (current != SKILL).then(|| fs::write(&path, SKILL).ok().map(|()| path)).flatten()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::Store;
    use crate::diff::Model;

    /// The example the skill teaches must import as exactly the finding it shows.
    #[test]
    fn the_skill_example_imports_as_a_finding() {
        let start = SKILL.find("```markdown\n").unwrap() + "```markdown\n".len();
        let example: String = SKILL[start..].split("   ```").next().unwrap().lines().map(str::trim_start).collect::<Vec<_>>().join("\n");
        let mut store = Store::default();
        let imported = store.import("review-7.md", &example, std::path::Path::new("/nonexistent"), &Model::default());
        assert_eq!(imported.added, 1);
        let f = &store.items[0];
        assert_eq!((f.severity.as_str(), f.title.as_str(), f.path.as_str(), f.line), ("major", "Hit counter is not thread-safe", "src/store/cache.rs", 17));
    }
}
