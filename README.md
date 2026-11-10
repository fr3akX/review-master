# review-master

Agent pull-request reviews posted as **line-anchored inline comments**, not one
review-body wall of text. Built in the mold of [cxplr](../cxplr): the review runs
on your choice of engine — Codex app-server, Claude Code, a native Anthropic
Messages client, or any OpenAI-compatible model — over a read-only checkout of
the PR head, and the findings land on GitHub as a single review whose comments
sit on the exact diff lines they talk about.

```sh
review-master 176                                  # PR #176 of this checkout's origin
review-master acme/widgets#176 --repo ~/src/widgets # explicit repo + checkout
review-master https://github.com/o/r/pull/7 --dry-run
```

## What a run does

1. Resolves the PR (URL, `owner/repo#N`, or a bare number against the checkout's
   GitHub `origin`) and fetches its metadata and unified diff through `gh`, so
   your existing GitHub CLI login is the only auth needed.
2. Fetches `pull/N/head` and checks it out into a detached temporary worktree
   (removed afterwards); a checkout already sitting on the PR head is used as is.
3. Runs one review session with the selected engine. The agent gets the diff and
   read-only, repository-confined tools to read surrounding context; it must
   answer with schema-checked findings (`path`, `line`/`start_line`, `side`,
   `severity`, `title`, `body`), a summary and a verdict.
4. Validates every anchor against the diff — GitHub only accepts comments on
   lines a hunk shows. Findings with bad anchors go back to the agent once for
   re-anchoring; whatever still fails is appended to the review body instead of
   being dropped (so one bad line number never 422s the whole review).
5. Posts one review: `COMMENT`, `APPROVE` or `REQUEST_CHANGES` (the model's
   verdict by default, `--event` overrides), pinned to the reviewed head commit.
   `--dry-run` prints the exact API payload instead.

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
| `PR` | | URL, `owner/repo#N`, `#N`, or `N` |
| `--repo DIR` | `.` | local checkout of the repository |
| `--engine` | configured, else `codex` | `codex` \| `claude` \| `anthropic` \| `openai` |
| `--model` | engine default | model for the engine |
| `--event` | `auto` | `auto` \| `comment` \| `approve` \| `request-changes` |
| `--dry-run` | | print the review and API payload, post nothing |
| `--exclude GLOB` | | drop files from the reviewed diff (repeatable, e.g. `--exclude '*.lock'`) |
| `--timeout-mins` | 20 | time limit for the review |
| `--cache-dir` | `.review-master` in the checkout | agent session storage |
| `--openai-base-url` | `$OPENAI_BASE_URL` | OpenAI-compatible API root |
| `--configure` | | interactive setup, then exit |
| `--anthropic-login` | | Claude subscription OAuth sign-in, then exit |

Diffs beyond ~384 KiB defer whole files into an "excluded" note (the agent is
told not to comment on them) rather than cutting a file mid-hunk; `--exclude`
generated or vendored files first for better reviews.

## Notes

- The agent is read-only and confined to the checkout: no shell (native
  engines), no network, symlink escapes rejected. Repository text is treated as
  untrusted data.
- GitHub refuses `APPROVE`/`REQUEST_CHANGES` on your own PR; use
  `--event comment` there.
- Engine sessions persist under the cache dir (native engines) or the engine's
  own store (Codex threads, Claude Code sessions), named
  `review-master: owner/repo#N`.
