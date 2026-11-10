# review-master

Code review in a local web UI: point it at a pull request (or a bare worktree),
it opens the browser with the changed files — diffs and full files with
highlighted changes — lets you **chat with an LLM about exact code lines**,
collect **draft inline comments** (your own, or an autonomous agent review's),
and submit everything as **one GitHub review** (comment / approve /
request-changes) with the comments anchored to the exact diff lines.

Built in the mold of [cxplr](../cxplr): the LLM side runs on your choice of
engine — Codex app-server, Claude Code, a native Anthropic Messages client, or
any OpenAI-compatible model — with read-only, repository-confined tools over a
checkout of the PR head.

```sh
review-master 176                                  # PR #176 of this checkout's origin → browser
review-master acme/widgets#176 --repo ~/src/widgets # explicit repo + checkout
review-master https://github.com/o/r/pull/7
review-master                                      # no PR: review this worktree vs the default branch
review-master 176 --headless --dry-run             # no UI: one agent review pass, print the payload
```

## What a run does

1. Resolves the PR (URL, `owner/repo#N`, or a bare number against the
   checkout's GitHub `origin`) and fetches metadata plus the unified diff
   through `gh` — your GitHub CLI login is the only auth. Without a PR, the
   worktree's change against its merge base with the default branch is reviewed
   (browse/chat/draft; submitting needs the PR).
2. Fetches `pull/N/head` into a detached temporary worktree (removed on exit);
   a checkout already on the PR head is used as is.
3. Serves the UI on localhost and opens the browser:
   - changed-file list, per-file **diff view** and **full-file view** with
     added lines highlighted and deletion markers, syntax highlighting;
   - select a line or range → **Comment** (draft an inline comment with a
     severity) or **Ask** (a chat thread with the engine, anchored to those
     lines; the engine can read the whole checkout read-only to answer);
   - **Auto review** runs the autonomous agent pass; its findings appear as
     draft comments like your own;
   - **Review** drawer: summary, event, all drafts → submit.
4. On submit, every draft is validated against the diff (GitHub only accepts
   comments on lines a hunk shows); drafts that cannot be anchored go into the
   review body instead of being dropped. One review is POSTed, pinned to the
   reviewed head commit. Drafts persist under `.review-master/` until then, so
   a restart keeps them.

`--headless` skips the UI: one agent review pass; bad anchors get one
re-anchoring turn; then the review is posted (`--event`, `--dry-run`).

## Requirements

- `gh` (GitHub CLI), logged in, and `git`.
- One engine:
  - `codex` on `PATH` with `codex app-server` (default), or
  - `claude` (Claude Code) on `PATH` for `--engine claude`, or
  - an Anthropic API key / Claude subscription OAuth login for
    `--engine anthropic` (`review-master --anthropic-login`), or
  - an OpenAI-compatible server with tool calling for `--engine openai`.

`review-master --configure` picks and checks the engine and model interactively
and saves them to `~/.config/review-master/config.toml`; the first interactive
launch without a configuration offers it too. Flags override the configuration,
and `$OPENAI_BASE_URL` / `$OPENAI_API_KEY` / `$OPENAI_MODEL` override the OpenAI
settings.

## Flags

| Flag | Default | |
| --- | --- | --- |
| `PR` | | URL, `owner/repo#N`, `#N`, or `N`; omitted = review the worktree |
| `--repo DIR` | `.` | local checkout of the repository |
| `--engine` | configured, else `codex` | `codex` \| `claude` \| `anthropic` \| `openai` |
| `--model` | engine default | model for the engine |
| `--host` / `--port` | `127.0.0.1` / `7979` | where the UI is served (busy port → a free one) |
| `--no-open` | | do not open the browser |
| `--headless` | | no UI: one agent review pass, post it (requires a PR) |
| `--event` | `auto` | with `--headless`: `auto` \| `comment` \| `approve` \| `request-changes` |
| `--dry-run` | | with `--headless`: print the API payload, post nothing |
| `--exclude GLOB` | | drop files from the reviewed diff (repeatable, e.g. `--exclude '*.lock'`) |
| `--timeout-mins` | 20 | time limit per agent turn (review or chat) |
| `--cache-dir` | `.review-master` in the checkout | sessions and drafts |
| `--openai-base-url` | `$OPENAI_BASE_URL` | OpenAI-compatible API root |
| `--configure` | | interactive setup, then exit |
| `--anthropic-login` | | Claude subscription OAuth sign-in, then exit |

Diffs beyond ~384 KiB defer whole files into an "excluded" note (the agent is
told not to comment on them, the UI lists them) rather than cutting a file
mid-hunk; `--exclude` generated or vendored files first for better reviews.

## Notes

- The agent is read-only and confined to the checkout: no shell (native
  engines), no network, symlink escapes rejected. Repository text is treated as
  untrusted data. The UI binds to loopback.
- GitHub refuses `APPROVE`/`REQUEST_CHANGES` on your own PR; the UI disables
  them there, `--headless` falls back with a clear error (`--event comment`).
- Engine sessions persist under the cache dir (native engines) or the engine's
  own store (Codex threads, Claude Code sessions), named
  `review-master: owner/repo#N` / `review-master: chat path:line`.
