# review-master

Code review for humans, with an LLM to help you understand the change. Point it
at a GitHub pull request, a GitLab merge request (gitlab.com or self-hosted), or
just a worktree path — the open request is resolved from the branch, its upstream,
or the head commit — and it opens a local web UI:

- a generated **PR summary** (why the change, things to note, change outline
  with clickable citations) appears at the top while you browse — cached per
  head commit, regenerate on demand;
- **PR flows**: on request, the LLM maps the requests, commands or jobs the change
  alters and you **walk each flow step by step** in execution order — every step
  highlights its lines in the diff (or the file viewer for code outside it) with a
  note on what happens there and what the PR changes; steps on changed lines are
  marked (see [Flows](#flows));
- **import another LLM's review** (a Markdown file, `--findings` or drag-and-drop):
  every finding that cites `path:line` is shown **on that line**, with its severity;
  turn the ones you agree with into draft comments, ask the chat about them, or
  dismiss them (see [LLM findings](#llm-findings)); an **agent skill** teaches Codex,
  Claude Code or any other agent to write reviews in that shape;
- browse the changed files in a collapsible, filterable **directory tree**;
  per-file **diff view** with word-level highlights inside changed lines and
  expandable context between hunks, and a **file viewer** for the whole file
  with highlighted changes and syntax highlighting;
- mark files **viewed** as you go — they collapse, the sidebar shows progress,
  and a mark resets when that file's diff changes (kept per PR);
- see the **review comments already on the PR** inline, replies threaded,
  outdated ones flagged — reply in a thread (posted right away), or ask the
  LLM whether a comment has a point;
- **keyboard-driven**: `j`/`k` files, `n`/`p` changes, `v` viewed, `/` filter,
  `]`/`[` flow steps, `Alt+←`/`→` back and forward through jumps, `?` for the full list;
- **fold `#[cfg(test)]` modules** in diffs and the file viewer — boundaries come
  from rust-analyzer's symbol tree (text scanning only as fallback); test-only
  hunks collapse to one bar, folds remember their state per PR;
- **exclude files from the review** right from the tree — a whole branch
  (`docs/`, generated code) or all test files with one toggle; excluded files
  leave the diff, the counts, and what the LLM is told about, and the set
  persists per PR (restore any of it from the `· N excluded` chip);
- **code navigation through language servers** (rust-analyzer, pyright,
  tsserver, gopls, clangd, Metals for Scala): ⌘/Ctrl-click an identifier to jump to
  its definition, Alt-click for hover, definitions and references grouped by file;
  whole-word text search as the fallback; an outline of the viewed file (from the
  server, or declaration patterns until it answers) that follows your scroll (see
  [Code navigation](#code-navigation));
- select code (lines or exact text) and **chat with an LLM about it**: it
  answers from a read-only checkout of the PR head, gets your exact selection,
  and its `path:line` citations are clickable jumps; a running answer can be
  stopped;
- write **draft inline comments** (severity + markdown) as you go; drafts,
  conversations and the unsent review summary persist across restarts;
- submit everything as **one review** — comment / approve / request-changes on
  GitHub, comment / approve on GitLab — with every comment anchored to the exact
  diff line;
  **Submit & exit** posts it and stops review-master in one go, **Exit** stops it
  without posting (drafts and everything else are kept for next time).

The tool never writes reviews for you; it explains, you judge.

The LLM side runs in the mold of [cxplr](../cxplr): Codex app-server, Claude
Code, a native Anthropic Messages client, or any OpenAI-compatible model, with
read-only repository-confined tools.

```sh
review-master 176                                  # PR #176 (or MR !176) of this checkout's origin → browser
review-master acme/widgets#176 --repo ~/src/widgets # explicit repo + checkout
review-master https://github.com/o/r/pull/7
review-master https://gitlab.example.com/group/project/-/merge_requests/12
review-master group/sub/project!12                 # GitLab merge request
review-master                                      # no request: review this worktree vs the default branch
```

## Install

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/fr3akX/review-master/releases/latest/download/review-master-installer.sh | sh
```

The installer downloads the prebuilt binary for your platform (macOS and Linux,
x86_64 and arm64) from the latest
[GitHub release](https://github.com/fr3akX/review-master/releases), checks its
checksum and puts `review-master` in `~/.cargo/bin` (it offers to add that to
your `PATH`).

From source, with a Rust toolchain:
`cargo install --git https://github.com/fr3akX/review-master --locked` (or
`cargo install --path . --locked` in a checkout).

### Updates

At most every 6 hours, a review-master installed by the installer checks GitHub
Releases in the background and prints a one-line notice when a newer version
exists:

```
review-master 0.2.0 is available (you have 0.1.0): run `review-master --update`
```

`review-master --update` downloads and installs it with the same installer.
`--no-update`, or `REVIEW_MASTER_NO_UPDATE=1`, skips the check. Builds from
source are not checked; update them by re-running `cargo install`. Installed
review-master skills are refreshed on the first start of a new version.

### Releasing

Releases are built by GitHub Actions with
[dist](https://github.com/axodotdev/cargo-dist)
(`.github/workflows/release.yml`, configured in `dist-workspace.toml`). To
publish:

```sh
# bump `version` in Cargo.toml, then:
cargo build && git commit -am "Release 0.2.0"
git tag v0.2.0 && git push origin main v0.2.0
```

The tag triggers builds for the four targets and a GitHub release with the
archives, checksums and `review-master-installer.sh`. After changing
`dist-workspace.toml`, run `dist generate` to update the workflow.

## What a run does

1. Resolves the request (URL, `owner/repo#N`, `project!N`, or a bare number
   against the checkout's `origin`) and fetches metadata plus the unified diff
   through `gh` (GitHub) or `glab` (GitLab) — your CLI login is the only auth.
   Without a request, the worktree's change against its merge base with the
   default branch is shown (browse/chat/draft; submitting needs the request).
2. Fetches `pull/N/head` (GitHub) or `merge-requests/N/head` (GitLab) into a
   detached temporary worktree (removed on exit), from whichever remote of the
   checkout points at the project (`origin`, or e.g. `upstream` in a fork); a
   checkout already on the head is used as is. Run anywhere else — another
   repository, or no repository at all — and only the reviewed commit is fetched
   into a temporary directory, over SSH first, then HTTPS, never prompting; its
   drafts and chats are kept under `~/.cache/review-master/<host>/<project>`
   (`$XDG_CACHE_HOME`) unless `--cache-dir` says otherwise.
3. Serves the UI on localhost and opens the browser.
4. On submit, every draft is validated against the diff (both forges only accept
   inline comments on lines a hunk shows; a range overhanging a hunk is trimmed to
   it); drafts that cannot be anchored are not dropped, and the drafts are cleared.
   GitHub: one review is POSTed, pinned to the reviewed head commit; a draft on lines
   the diff does not show becomes a comment on its whole file (the review is then
   created pending, gets those comments, and is submitted). GitLab: such drafts go
   into the review body; see [GitLab](#gitlab).
5. Runs until **Exit** (or **Submit & exit**) in the UI, or Ctrl-C in the terminal;
   then the temporary worktree is removed. Browsers only let a page close tabs a
   script opened, so the tab the terminal opened stays, saying review-master
   stopped.

## Requirements

- `git`, and the forge's CLI, logged in: `gh` (GitHub CLI) or `glab` (GitLab CLI,
  `glab auth login --hostname <your GitLab>` for self-hosted instances).
- One engine for the chat:
  - `codex` on `PATH` with `codex app-server` (default), or
  - `claude` (Claude Code) on `PATH` for `--engine claude`, or
  - an Anthropic API key / Claude subscription OAuth login for
    `--engine anthropic` (`review-master --anthropic-login`), or
  - an OpenAI-compatible server with tool calling for `--engine openai`.

`review-master --configure` picks and checks the engine, model and reasoning effort interactively
and saves them to `~/.config/review-master/config.toml`; the first interactive
launch without a configuration offers it too. Flags override the configuration,
and `$OPENAI_BASE_URL` / `$OPENAI_API_KEY` / `$OPENAI_MODEL` override the OpenAI
settings.

### Reasoning effort

`--effort` (or `effort` in the configuration, asked by `--configure`) sets how hard the
engine reasons for chats, the summary and flows; without it each engine uses its own
default. The word goes to the engine, which checks it:

| Engine | Sent as | Levels | Without `--effort` |
| --- | --- | --- | --- |
| `codex` | the turn's `effort` | what the model offers (e.g. `low`, `medium`, `high`) | `model_reasoning_effort` in your Codex config |
| `claude` | `claude -p --effort` | `low`, `medium`, `high`, `xhigh`, `max` | Claude Code's default (`--restricted` ignores your settings files) |
| `anthropic` | Claude 5 / Opus and Sonnet 4.6+: adaptive thinking with `output_config.effort`; older models: a thinking budget | `low`, `medium`, `high`, `xhigh`, `max`; `off` only before Claude 4.6 | the API's default |
| `openai` | `reasoning_effort` | the server's, e.g. `minimal`…`high`, or `none` (thinking off) on llama.cpp Qwen templates | the server's |

A configured effort belongs to its engine: `--engine claude` does not inherit the
effort configured for `codex`. The startup line and the header chip show it.

## Flags

| Flag | Default | |
| --- | --- | --- |
| `PR` | | URL, `owner/repo#N`, `project!N`, `#N`/`!N`, or `N`; omitted = review the worktree |
| `--forge` | from the origin | `github` \| `gitlab`, for an origin whose host name does not say and that `glab` is not logged in to |
| `--repo DIR` | `.` | local checkout of the repository |
| `--engine` | configured, else `codex` | `codex` \| `claude` \| `anthropic` \| `openai` |
| `--model` | engine default | model for the chat engine |
| `--effort LEVEL` | configured, else engine default | reasoning effort (see [Reasoning effort](#reasoning-effort)) |
| `--host` / `--port` | `127.0.0.1` / `7979` | where the UI is served (busy port → a free one) |
| `--no-open` | | do not open the browser |
| `--lsp LANG=COMMAND` | auto-detected on PATH | language server override; empty command disables one language |
| `--no-lsp` | | disable language servers (navigation falls back to text search) |
| `--lsp-import-build` | | let Metals import the build (runs the build tool) |
| `--findings FILE` | | Markdown review whose `path:line` findings annotate the code (repeatable) |
| `--install-skill [AGENT]` | every agent found | install the agent skill for `codex`, `claude` or `all`, then exit |
| `--skill` | | print the agent skill (SKILL.md), then exit |
| `--exclude GLOB` | | drop files from the reviewed diff (repeatable, e.g. `--exclude '*.lock'`) |
| `--timeout-mins` | 20 | time limit per chat turn |
| `--cache-dir` | `.review-master` in the checkout | chat sessions and drafts |
| `--openai-base-url` | `$OPENAI_BASE_URL` | OpenAI-compatible API root |
| `--configure` | | interactive setup, then exit |
| `--anthropic-login` | | Claude subscription OAuth sign-in, then exit |
| `--update` | | install the latest release now (installer installs), then exit |
| `--no-update` | | skip the check for a newer release (also `REVIEW_MASTER_NO_UPDATE=1`) |

## Code navigation

- ⌘/Ctrl-click an identifier (diff or file viewer): jump to its definition when the
  language server knows exactly one in the checkout, else the symbol popup.
- Alt-click: the popup — hover text, definitions, **References** (grouped by file) and
  **Text search** (whole-word matches across the checkout). Deleted lines are not in the
  head checkout, so they only get text search.
- Jumps (definitions, references, outline entries, `path:line` links in chat and
  summaries) are recorded: `Alt+←` / `Alt+→` or **‹ ›** in the viewer go back and forth,
  back to the scroll position you left.
- **Outline** in the file viewer: the file's functions, classes and other declarations,
  filterable, with the one you are reading marked as you scroll. It comes from the
  language server once it answers; until then, and for files without one, declaration
  patterns cover Rust, Python, JS/TS, Go, Java/Kotlin/C#/Swift, Scala, C/C++, Ruby, PHP,
  shell and Markdown headings. The footer says which.
- The viewer chip shows the server and its state (`starting…`, `indexing…`, ready,
  `unavailable`, or `text search` for files no server handles).

Servers are found on `PATH` and started on first use, one per language, in the
reviewed checkout:

| Language | Servers tried |
| --- | --- |
| Rust | `rust-analyzer` (falls back to the default rustup toolchain when the pinned one lacks the component) |
| Python | `pyright-langserver --stdio`, `basedpyright-langserver --stdio`, `pylsp`, `jedi-language-server` |
| TypeScript / JavaScript | `typescript-language-server --stdio` |
| Go | `gopls` |
| C / C++ | `clangd` |
| Scala (`.scala`, `.sc`, `.sbt`) | `metals` |

`--lsp LANG=COMMAND` overrides one (`--lsp scala="cs launch metals"`), an empty command
disables it, `--no-lsp` disables all. Language servers can run project tooling; use
`--no-lsp` on repositories you do not trust.

### Scala

Install Metals with [coursier](https://get-coursier.io) (needs a JDK): `cs install metals`.
review-master finds it on `PATH` or, if coursier's bin directory is not on `PATH`, there
(`$COURSIER_BIN_DIR`, `~/.local/share/coursier/bin`, `~/Library/Application
Support/Coursier/bin`). When a change has files of a language whose server is not
installed, the startup output says so with the install command, and the viewer's chip
reads `scala: no language server` instead of falling back silently.
By default review-master declines Metals' build import, so nothing in the project runs:
the outline and hover within a file work, while definitions and references across files
fall back to text search. `--lsp-import-build` imports the build (sbt, Mill, Gradle, …
through Bloop), which runs the build definition, downloads dependencies and compiles —
minutes the first time. In PR mode the checkout is a temporary worktree that is removed
on exit, so the import (`.bloop/`, `.metals/`) is redone on every run; review from a
checkout that is already on the PR head (or pass its path) to keep it. Metals' and
Bloop's directories stay out of searches and the LLM's tools. Test files by JVM naming
(`FooSpec.scala`, `FooSuite.scala`, `FooTest.java`, `src/it/…`) count as tests for
**Exclude tests**.

## Flows

The **Flows** card under the summary maps the change in execution order. **Map flows**
asks the engine (read-only, a minute or so) for the 1–4 end-to-end paths — a request,
command, event or job — whose behavior the change adds or alters, each as 3–25 steps
from the entry point to the outcome. Every step is a line range at the PR head with
what the code does there and what the PR changes at that step; ranges are checked
against the files (made-up ones are flagged), and steps that touch added or removed
lines are marked ●. The result is cached per head commit; ↻ regenerates it.

**▶ Walk** (or a step in the list) starts the walkthrough: the step's lines are
highlighted in the file's diff, or in the file viewer when they are outside it, with a
note below them. `]` / `[` (or ◀ ▶) step, `Space` plays with the chosen speed, `Esc`
leaves; `Alt+←` then returns to where the walk started.

## LLM findings

Have another model review the change, then import its Markdown: `--findings
review.md` (repeatable), **Import Markdown…** or paste in the **Findings** drawer (`f`),
or drop the file on the page. Importing the same file name again replaces its
findings and keeps what you dismissed. Starting with `--findings` replaces every
earlier import of this review, so a re-run saved under a new name does not leave the
previous run's findings behind.

The review can take most shapes models produce: a heading per finding, bullets under
severity headings (`## Critical`, `## Nits`), or fields (`**Location:** …`,
`**Severity:** High`). A finding is a section or bullet that cites a location —
`src/a.rs:12`, `a.rs:12-18`, `path#L12-L18`, `path (lines 12-18)`, GitHub blob URLs.
Paths are matched to the checkout through diff prefixes (`a/`, `b/`), longer paths
and bare names of changed files. Severities map to blocker / major / minor / nit
(`critical`, `high`, `low`, `P1`, 🔴 … are understood); sections without a location
and overview sections (Summary, Verdict, …) are listed as not imported.

Each finding sits on its line in the diff (or the viewer), and in the sidebar count
(`⚑`). **Draft** opens a comment prefilled with it — you edit and save it like any
draft; **Ask** opens a chat about it; **Dismiss** hides it. The drawer lists them all,
flags locations that could not be placed, and restores dismissed ones. Nothing from an
imported review is posted unless you save it as a draft.

### The agent skill

The review-master skill tells a coding agent how to write a review that imports
cleanly: a `### [severity] title (path:line)` heading per finding, head-commit line
numbers taken from the diff or a numbered read, lines the diff shows where possible
(they can become inline comments), only findings, and the `review-master <N>
--findings <file>` command to open the result.

```sh
review-master --install-skill          # every agent found: ~/.codex and/or ~/.claude
review-master --install-skill claude   # $CLAUDE_CONFIG_DIR/skills/review-master/SKILL.md (default ~/.claude)
review-master --install-skill codex    # $CODEX_HOME/skills/review-master/SKILL.md (default ~/.codex)
```

Then ask the agent to "review PR 42 for review-master". Installed skills are kept in
step with review-master: it rewrites them when it starts and their text changed.

`--skill` prints the same text, for other agents and other places:

```sh
mkdir -p .claude/skills/review-master && review-master --skill > .claude/skills/review-master/SKILL.md  # project-local
claude -p "$(review-master --skill) Review PR 42."   # writes /tmp/review-42.md, as the skill says
```

The agent decides when to load a skill, so this is guidance, not enforcement.

## GitLab

Merge requests work like pull requests, through `glab api` on the host the request
lives on. The forge comes from the reference or URL (`!N`, `/-/merge_requests/N`), or
from the checkout's `origin`: github.com is GitHub; a host with `gitlab` in its name, or
one `glab` is logged in to, is GitLab; `--forge gitlab` settles anything else. Projects
in subgroups (`group/sub/project`) are fine.

- **Diff**: the merge request's latest diff version, per file from the API, rebuilt as
  one unified diff.
- **Existing comments**: diff discussions with their replies; general comments and
  system notes are left out. A discussion whose position is not on the latest diff
  version is shown as outdated. **Reply** posts into the discussion right away.
- **Submit**: every inline draft becomes a draft note on its line (an unchanged line
  carries both its old and new number, as GitLab requires), the summary a general
  draft note, and all of them are published at once as your review. A multi-line draft
  is anchored on its last line and names the range. **Approve** then approves the
  merge request at the reviewed head; if only the approval fails (e.g. the project
  forbids approving your own merge request), the review stays published and the UI
  says why. GitLab's API has no request-changes review, so that option is off.
  A draft GitLab rejects goes into the summary like any unanchorable draft; if
  publishing itself fails, the notes wait as your pending review on GitLab.

## Tests

`cargo test` covers the Rust side. `tests/ui/run.sh` drives the real binary and
web UI in headless Chrome against a demo repository, a fake `gh`, a fake `glab` and a fake
engine — no network, nothing is posted (needs node ≥ 22 and Chrome; `$CHROME`
overrides the macOS default path, `--keep` leaves screenshots behind).

## Notes

- The chat engine is read-only and confined to the checkout: no shell (native
  engines), no network, symlink escapes rejected. Repository text is treated as
  untrusted data. The UI binds to loopback.
- GitHub refuses `APPROVE`/`REQUEST_CHANGES` on your own PR; the UI disables
  them there and says why.
- Chat sessions persist under the cache dir (native engines) or the engine's
  own store (Codex threads, Claude Code sessions), named
  `review-master: chat path:line`; follow-ups continue their session. The
  conversation list itself is kept in the cache dir, so it is back after a
  restart; delete a conversation from the chat drawer.
- Viewed marks, panel widths and fold choices live in the browser's
  localStorage; drafts, conversations, scope, the summary, flows and imported
  findings in the cache dir.
