#!/bin/bash
# UI tests: drives the real binary and web UI in headless Chrome against a demo repository,
# a fake `gh` and a fake OpenAI-compatible engine — no network, nothing posted anywhere.
# Needs node >= 22 and Chrome ($CHROME overrides the macOS default path).
# usage: tests/ui/run.sh [--keep]     (--keep leaves the work dir, with screenshots, behind)
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/review-master-ui.XXXXXX")
UI_PORT=${UI_PORT:-17981}
ENGINE_PORT=${ENGINE_PORT:-18765}
export FAKE_GH_REPO=$WORK/demo FAKE_GH_OUT=$WORK/out/review.json SHOTS=$WORK/shots
export PATH=$HERE/bin:$PATH OPENAI_API_KEY= OPENAI_MODEL=
mkdir -p "$WORK/out" "$SHOTS"
SERVER=; ENGINE=; FAILED=0

stop_server() { [ -n "$SERVER" ] && kill "$SERVER" 2>/dev/null && wait "$SERVER" 2>/dev/null; SERVER=; }
exited() { # the suite pressed Exit: the server process must end by itself
  for _ in $(seq 50); do kill -0 "$SERVER" 2>/dev/null || { wait "$SERVER"; SERVER=; return 0; }; sleep 0.1; done
  echo "   server still running after Exit"; FAILED=1; stop_server
}
cleanup() {
  stop_server
  [ -n "$ENGINE" ] && kill "$ENGINE" 2>/dev/null
  if [ "${1:-}" = "--keep" ]; then echo "work dir: $WORK"; else rm -rf "$WORK"; fi
}
trap 'cleanup "${KEEP:-}"' EXIT
[ "${1:-}" = "--keep" ] && KEEP=--keep

start_server() { # <cache name> [review-master args…]
  local cache=$1; shift
  (cd "$WORK/demo" && exec "$ROOT/target/debug/review-master" --engine openai --model fake \
    --openai-base-url "http://127.0.0.1:$ENGINE_PORT/v1" --no-open --no-lsp --port "$UI_PORT" \
    --cache-dir "$WORK/cache-$cache" "$@") > "$WORK/server.log" 2>&1 &
  SERVER=$!
  for _ in $(seq 50); do curl -fs "http://127.0.0.1:$UI_PORT/api/info" > /dev/null && return 0; sleep 0.1; done
  echo "server did not start:"; cat "$WORK/server.log"; exit 1
}
suite() { # <name> [cdp args…]
  local name=$1; shift
  echo "── $name"
  node "$HERE/cdp.mjs" "http://127.0.0.1:$UI_PORT" "$HERE/$name.mjs" "$@" > "$WORK/$name.log" 2>&1 || { FAILED=1; echo "   $name FAILED"; }
  grep -v '^ok ' "$WORK/$name.log"
}

(cd "$ROOT" && cargo build --quiet) || exit 1
echo "── unit"
node "$HERE/unit.mjs" > "$WORK/unit.log" 2>&1 || { FAILED=1; echo "   unit FAILED"; }
grep -v '^ok ' "$WORK/unit.log"
"$HERE/demo-repo.sh" "$WORK/demo"
node "$HERE/fake-openai.mjs" "$ENGINE_PORT" > "$WORK/engine.log" 2>&1 &
ENGINE=$!

start_server pr;                               suite suite-main;            stop_server
FAKE_GH_COMMENTS_FAIL=1 start_server pr;       suite suite-restart --dark;  exited
git -C "$WORK/demo" remote remove origin
start_server local;                            suite suite-local;           exited
git -C "$WORK/demo" remote add origin https://gitlab.example.com/acme/demo.git
start_server gitlab;                           suite suite-gitlab;          stop_server

[ $FAILED -eq 0 ] && echo "UI tests passed" || echo "UI tests FAILED"
exit $FAILED
