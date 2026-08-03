#!/usr/bin/env bash
# Hook binary budget report (plan §Measurement design, "Binary size and
# dynamic-link cost").
#
# A daemon amortizes its binary over one execve. A hook pays for its binary on
# EVERY invocation, so size, the shared-object set and the relocation count are
# first-class budget items here in a way they never were for memgardend.
#
# Four checks. **Only #3 is wired into CI** (.github/workflows/ci.yml); #1, #2
# and #4 are human checks whose output belongs in the PR body. A green CI does
# not prove them — say which is which whenever they are cited.
#
#   1. size          <= 8 MB      tripwire, not a squeeze
#   2. ldd           glibc baseline only, and nothing heavy
#   3. cargo tree    containment  <- the CI-wired one
#   4. LD_DEBUG      read on regression only, never a gate
#
# Usage: scripts/hook-budget.sh [path-to-memgarden-binary]
#        (builds the release binary if no path is given)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-}"
FAILED=0

note() { printf '\n== %s ==\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1"; FAILED=1; }
pass() { printf 'ok:   %s\n' "$1"; }

if [[ -z "$BIN" ]]; then
    note "build"
    cargo build --release -p memgarden-cli --bins --manifest-path "$REPO_ROOT/Cargo.toml" \
        || { echo "build failed"; exit 1; }
    BIN="$REPO_ROOT/target/release/memgarden"
fi
[[ -x "$BIN" ]] || { echo "not executable: $BIN"; exit 1; }

# --- 1. size (human check) -------------------------------------------------
# 8 MB against a measured 606 KB prototype: a tripwire for "something heavy got
# linked", not a number anyone should be optimizing towards.
note "1. size (human check, budget 8 MB)"
SIZE_BYTES=$(stat -c %s "$BIN")
awk -v b="$SIZE_BYTES" -v p="$BIN" 'BEGIN { printf "%s  %d bytes (%.2f MB)\n", p, b, b/1048576 }'
if (( SIZE_BYTES > 8 * 1024 * 1024 )); then
    fail "binary exceeds the 8 MB budget"
else
    pass "within the 8 MB budget"
fi

# --- 2. shared objects (human check) ---------------------------------------
# Static musl was considered and rejected: total process cost is already
# 0.34 ms, so there is nothing to buy.
note "2. ldd (human check)"
LDD_OUT=$(ldd "$BIN" 2>&1)
printf '%s\n' "$LDD_OUT"
LDD_DIRTY=0
for forbidden in libssl libcrypto libonnxruntime libsqlite3 libstdc++; do
    if grep -qF "$forbidden" <<<"$LDD_OUT"; then
        fail "links $forbidden"
        LDD_DIRTY=1
    fi
done
(( LDD_DIRTY == 0 )) && pass "no TLS / onnxruntime / sqlite / libstdc++ in the link set"

# --- 3. dependency closure (CI-wired) --------------------------------------
# The check that will actually fire one day: a stray `use memgarden_store::…`
# compiles fine and silently adds a 90 s C build and 1.5 MB of SQLite to every
# hook process. `indexmap` is on the list because its appearance would mean
# serde_json's `preserve_order` got switched on somewhere — which Cargo feature
# unification would then apply to memgardend, reordering every API response.
note "3. cargo tree containment (CI-wired)"
TREE=$(cd "$REPO_ROOT" && cargo tree -p memgarden-cli --edges normal 2>&1)
TREE_DIRTY=0
for forbidden in rusqlite libsqlite3-sys fastembed ort tokio reqwest hf-hub \
                 tiktoken-rs sqlite-vec axum clap indexmap; do
    if grep -qE "(^|[^a-zA-Z0-9_-])${forbidden} v" <<<"$TREE"; then
        fail "memgarden-cli depends on $forbidden"
        TREE_DIRTY=1
    fi
done
printf '%s\n' "$TREE"
(( TREE_DIRTY == 0 )) && pass "dependency closure is clean"

# --- 4. link-time explanation (on regression only) -------------------------
# Not a gate. Arm B's paired p50 (budget 1.5 ms) is the behavioural number;
# this is what you read when arm B moves.
note "4. LD_DEBUG=statistics (diagnostic only, never a gate)"
LD_DEBUG=statistics "$BIN" hook noop </dev/null 2>&1 | sed -n '1,20p'

note "result"
if (( FAILED )); then
    echo "budget FAILED"
    exit 1
fi
echo "budget ok"
