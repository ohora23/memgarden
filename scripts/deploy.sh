#!/usr/bin/env bash
# scripts/deploy.sh — build, back up if the schema moves, install both
# binaries, restart, and verify by build identity (DP-1 D2).
#
# Every step prints what it observed. The #57 deploy was verified by PID and
# uptime because nothing could say what was running; this script refuses to
# finish until /healthz reports the commit it just built.
#
#   scripts/deploy.sh            # from a clean checkout at the commit to deploy
#   ALLOW_DIRTY=1 scripts/deploy.sh   # install a -dirty build anyway (not for prod)
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

URL=${MEMGARDEN_URL:-http://127.0.0.1:9100}
TOKEN_FILE=${MEMGARDEN_TOKEN_FILE:-$HOME/.local/share/memgarden/daemon.token}
BIN=${CARGO_HOME:-$HOME/.cargo}/bin
SERVICE=memgardend.service

say() { printf '\n== %s\n' "$*"; }
healthz() { curl -sf -H "Authorization: Bearer $(cat "$TOKEN_FILE")" "$URL/healthz"; }
jsonfield() { python3 -c 'import json,sys; print(json.load(sys.stdin).get(sys.argv[1], ""))' "$1"; }

# 0. What are we deploying?
say "commit"
if [ -n "$(git status --porcelain)" ] && [ "${ALLOW_DIRTY:-0}" != 1 ]; then
  echo "working tree is dirty; a -dirty build cannot be traced back to a commit. Commit, or ALLOW_DIRTY=1."
  exit 1
fi
SHA=$(git rev-parse --short HEAD)
echo "HEAD $SHA ($(git log -1 --format=%s | cut -c1-70))"

# 1. Both binaries, always. Installing one of two is how #57 shipped a skew.
say "build"
cargo build --release --bin memgardend --bin memgarden
./target/release/memgardend --version

# 2. The schema this build wants, against the file the daemon is running on.
say "schema"
WANT=$(./target/release/memgardend --version | sed -E 's/.*schema v([0-9]+).*/\1/')
LIVE=$(healthz || true)
if [ -z "$LIVE" ]; then
  echo "daemon not answering at $URL; deploying anyway, no backup decision possible without its db_path"
  DB=""
else
  DB=$(printf '%s' "$LIVE" | jsonfield db_path)
  RUNNING=$(printf '%s' "$LIVE" | jsonfield build)
  HAVE=$(python3 -c 'import sqlite3,sys; print(sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True).execute("pragma user_version").fetchone()[0])' "$DB")
  echo "running build ${RUNNING:-unknown}; db $DB at v$HAVE; this build wants v$WANT"
  if [ "$WANT" -gt "$HAVE" ]; then
    # 3. Back up *because* the version changed. VACUUM INTO is a consistent
    #    snapshot under WAL and does not stop the daemon.
    BACKUP="$(dirname "$DB")/backup-pre-v${WANT}-$(date -u +%Y%m%dT%H%M%SZ).db"
    say "backup (schema v$HAVE -> v$WANT)"
    python3 -c 'import sqlite3,sys; sqlite3.connect(sys.argv[1]).execute(f"VACUUM INTO \x27{sys.argv[2]}\x27")' "$DB" "$BACKUP"
    ls -la "$BACKUP"
  elif [ "$WANT" -lt "$HAVE" ]; then
    echo "this build (v$WANT) is OLDER than the database (v$HAVE): it will refuse to start. Restore the backup first."
    exit 1
  fi
fi

# 4. Install: write beside, then rename — a reader never sees a half file.
say "install"
for b in memgardend memgarden; do
  cp "target/release/$b" "$BIN/$b.new" && mv -f "$BIN/$b.new" "$BIN/$b"
  echo "$BIN/$b <- target/release/$b"
done

# 5. Restart. With memgardend.socket installed the port never closes.
say "restart"
if systemctl --user is-active --quiet "$SERVICE"; then
  systemctl --user restart "$SERVICE"
  systemctl --user is-active memgardend.socket >/dev/null 2>&1 || echo "note: no memgardend.socket — hooks in the restart window were refused (scripts/systemd/)"
else
  echo "$SERVICE is not running under systemd --user; start the new binary yourself: $BIN/memgardend"
fi

# 6. Verify by identity, not liveness.
say "verify"
H='{}'
for _ in $(seq 1 60); do
  H=$(healthz || echo '{}')
  if [ "$(printf '%s' "$H" | jsonfield build)" = "$SHA" ]; then break; fi
  sleep 0.5
done
STATUS=$(printf '%s' "$H" | jsonfield status)
GOT=$(printf '%s' "$H" | jsonfield build)
echo "/healthz status=$STATUS build=$GOT (wanted $SHA)"
[ "$GOT" = "$SHA" ] || { echo "the running daemon is not this build"; exit 1; }

# 7. The hook binary agrees with the daemon.
"$BIN/memgarden" hooks status 2>/dev/null | grep -E 'build:' || true
"$BIN/memgarden" hooks status 2>/dev/null | grep -q 'daemon and this binary' || { echo "hook binary and daemon differ"; exit 1; }

# 8. What the restart cost.
say "restart cost"
journalctl --user -u "$SERVICE" --since '-2 min' --no-pager 2>/dev/null | grep -E 'closed out|listening' | tail -3 || true
say "deployed $SHA"
