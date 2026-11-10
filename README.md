# review-master

Code review for humans, with an LLM to help you understand the change. Point it
at a pull request (or a bare worktree) and it opens a local web UI:

- browse the changed files — per-file **diff view** and **full-file view** with
  highlighted changes and syntax highlighting;
- select a line or range and **chat with an LLM about exactly that code**: it
  answers from a read-only checkout of the PR head, so it can read the
  surrounding code, and cites `path:line`;
- write **draft inline comments** (severity + markdown) as you go; drafts
  persist across restarts;
- submit everything as **one GitHub review** — comment / approve /
  request-changes — with every comment anchored to the exact diff line.

The tool never writes reviews for you; it explains, you judge.

The LLM side runs in the mold of [cxplr](../cxplr): Codex app-server, Claude
Code, a native Anthropic Messages client, or any OpenAI-compatible model, with
read-only repository-confined tools.

```sh
review-master 176                                  # PR #176 of this checkout's origin → browser
review-master acme/widgets#176 --repo ~/src/widgets # explicit repo + checkout
review-master https://github.com/o/r/pull/7
review-master                                      # no PR: review this worktree vs the default branch
```

## What a run does

1. Resolves the PR (URL, `owner/repo#N`, or a bare number against the
   checkout's GitHub `origin`) and fetches metadata plus the unified diff
   through `gh` — your GitHub CLI login is the only auth. Without a PR, the
   worktree's change against its merge base with the default branch is shown
   (browse/chat/draft; submitting needs the PR).
2. Fetches `pull/N/head` into a detached temporary worktree (removed on exit);
   a checkout already on the PR head is used as is.
3. Serves the UI on localhost and opens the browser.
4. On submit, every draft is validated against the diff (GitHub only accepts
   comments on lines a hunk shows); drafts that cannot be anchored go into the
   review body instead of being dropped. One review is POSTed, pinned to the
   reviewed head commit, and the drafts are cleared.

## Requirements

- `gh` (GitHub CLI), logged in, and `git`.
- One engine for the chat:
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
| `--model` | engine default | model for the chat engine |
| `--host` / `--port` | `127.0.0.1` / `7979` | where the UI is served (busy port → a free one) |
| `--no-open` | | do not open the browser |
| `--exclude GLOB` | | drop files from the reviewed diff (repeatable, e.g. `--exclude '*.lock'`) |
| `--timeout-mins` | 20 | time limit per chat turn |
| `--cache-dir` | `.review-master` in the checkout | chat sessions and drafts |
| `--openai-base-url` | `$OPENAI_BASE_URL` | OpenAI-compatible API root |
| `--configure` | | interactive setup, then exit |
| `--anthropic-login` | | Claude subscription OAuth sign-in, then exit |

## Notes

- The chat engine is read-only and confined to the checkout: no shell (native
  engines), no network, symlink escapes rejected. Repository text is treated as
  untrusted data. The UI binds to loopback.
- GitHub refuses `APPROVE`/`REQUEST_CHANGES` on your own PR; the UI disables
  them there and says why.
- Chat sessions persist under the cache dir (native engines) or the engine's
  own store (Codex threads, Claude Code sessions), named
  `review-master: chat path:line`; follow-ups continue their session.
