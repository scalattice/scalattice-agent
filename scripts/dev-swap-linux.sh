#!/usr/bin/env bash
# Local canary ONLY on this machine: build → loopback WS → swap binary → restart.
#
# NOT for production agents / fleet installs / release shipping:
#   - Does not publish GitHub releases or update other machines.
#   - Local mode sets SCALATTICE_WS_URL to loopback ws:// only; the agent
#     rejects non-loopback / wss overrides, so this cannot redirect a prod
#     agent to an arbitrary host.
#   - Shipping to prod uses the normal release + install path, not this script.
#   - Use --cloud on this box to clear the loopback override and use the
#     default cloud endpoint again.
#   - Stops every agent runtime before swap and asserts exactly one parent
#     after restart (systemd only). Never leaves a second GPU owner running.
#
# Usage:
#   ./scripts/dev-swap-linux.sh                 # local WS + build + swap + restart
#   ./scripts/dev-swap-linux.sh --skip-build    # swap existing target/release binary
#   ./scripts/dev-swap-linux.sh --cloud          # clear local WS override (cloud default)
#   SCALATTICE_VERSION=1.1.141 ./scripts/dev-swap-linux.sh
#   SCALATTICE_WS_URL=ws://127.0.0.1:… ./scripts/dev-swap-linux.sh
#
# Local mode writes SCALATTICE_WS_URL into ~/.config/scalattice/agent.systemd.env
# (and agent.env).
set -euo pipefail

cd "$(dirname "$0")/.."

SKIP_BUILD=0
CLOUD_MODE=0
for arg in "$@"; do
  case "$arg" in
    --skip-build) SKIP_BUILD=1 ;;
    --cloud|--prod) CLOUD_MODE=1 ;;
    --local) CLOUD_MODE=0 ;;
    -h|--help)
      sed -n '2,20p' "$0"
      exit 0
      ;;
    *)
      echo "Unknown arg: $arg (try --skip-build, --cloud, --local)" >&2
      exit 2
      ;;
  esac
done

UNIT="scalattice-agent.service"
WATCHDOG_TIMER="scalattice-agent-watchdog.timer"
# Honor CARGO_TARGET_DIR (Cursor sandbox / CI often redirect away from ./target).
SRC="${CARGO_TARGET_DIR:-target}/release/scalattice-agent"
DEST="${HOME}/.local/bin/scalattice-agent"
CFG_DIR="${HOME}/.config/scalattice"
SYSTEMD_ENV="${CFG_DIR}/agent.systemd.env"
AGENT_ENV="${CFG_DIR}/agent.env"
LOCAL_WS_DEFAULT="ws://127.0.0.1:8080/v1/providers/agent/ws"

upsert_env_key() {
  local file="$1" key="$2" value="$3"
  mkdir -p "$(dirname "$file")"
  touch "$file"
  if grep -qE "^${key}=" "$file" 2>/dev/null; then
    # Avoid sed -i portability issues; rewrite via temp.
    local tmp
    tmp="$(mktemp)"
    awk -v k="$key" -v v="$value" '
      BEGIN { done=0 }
      $0 ~ ("^" k "=") { print k "=" v; done=1; next }
      { print }
      END { if (!done) print k "=" v }
    ' "$file" >"$tmp"
    mv "$tmp" "$file"
  else
    printf '%s=%s\n' "$key" "$value" >>"$file"
  fi
}

remove_env_key() {
  local file="$1" key="$2"
  [[ -f "$file" ]] || return 0
  local tmp
  tmp="$(mktemp)"
  grep -vE "^${key}=" "$file" >"$tmp" || true
  mv "$tmp" "$file"
}

configure_ws_target() {
  mkdir -p "$CFG_DIR"
  # Keep token present in both env files if only one has it.
  local token=""
  token="$(grep -E '^SCALATTICE_AGENT_TOKEN=' "$SYSTEMD_ENV" 2>/dev/null | head -1 | cut -d= -f2- || true)"
  if [[ -z "$token" ]]; then
    token="$(grep -E '^SCALATTICE_AGENT_TOKEN=' "$AGENT_ENV" 2>/dev/null | head -1 | cut -d= -f2- || true)"
  fi

  if [[ "$CLOUD_MODE" -eq 1 ]]; then
    echo "==> cloud mode: clearing SCALATTICE_WS_URL (use built-in cloud default)"
    remove_env_key "$SYSTEMD_ENV" "SCALATTICE_WS_URL"
    remove_env_key "$AGENT_ENV" "SCALATTICE_WS_URL"
    rm -f "${CFG_DIR}/dev-local"
  else
    local ws="${SCALATTICE_WS_URL:-$LOCAL_WS_DEFAULT}"
    # Port is optional; host must be loopback (agent enforces the same).
    if ! [[ "$ws" =~ ^ws://(127\.0\.0\.1|localhost|\[::1\])(:[0-9]+)?(/|$) ]]; then
      echo "error: SCALATTICE_WS_URL must be loopback ws:// (got: $ws)" >&2
      echo "       agent rejects non-loopback / wss overrides for safety." >&2
      exit 1
    fi
    echo "==> local mode: SCALATTICE_WS_URL=$ws"
    upsert_env_key "$SYSTEMD_ENV" "SCALATTICE_WS_URL" "$ws"
    upsert_env_key "$AGENT_ENV" "SCALATTICE_WS_URL" "$ws"
    date -u +%Y-%m-%dT%H:%M:%SZ >"${CFG_DIR}/dev-local"
  fi

  if [[ -n "$token" ]]; then
    upsert_env_key "$SYSTEMD_ENV" "SCALATTICE_AGENT_TOKEN" "$token"
    upsert_env_key "$AGENT_ENV" "SCALATTICE_AGENT_TOKEN" "$token"
  fi

  # systemd EnvironmentFile needs PATH/LD_LIBRARY_PATH too
  if [[ -f "$SYSTEMD_ENV" ]]; then
    grep -qE '^PATH=' "$SYSTEMD_ENV" 2>/dev/null \
      || upsert_env_key "$SYSTEMD_ENV" "PATH" "${HOME}/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    grep -qE '^LD_LIBRARY_PATH=' "$SYSTEMD_ENV" 2>/dev/null \
      || upsert_env_key "$SYSTEMD_ENV" "LD_LIBRARY_PATH" "${HOME}/.local/lib/scalattice:\${LD_LIBRARY_PATH:-}"
  fi
}

if [[ -z "${SCALATTICE_VERSION:-}" ]]; then
  # Prefer latest published tip so local admin does not flag "agent outdated".
  TIP="$(git ls-remote --tags origin 'refs/tags/v*' 2>/dev/null \
    | awk '{print $2}' | sed 's|refs/tags/||; s/\^{}$//' \
    | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sed 's/^v//' \
    | sort -t. -k1,1n -k2,2n -k3,3n | tail -1 || true)"
  if [[ -n "${TIP:-}" ]]; then
    export SCALATTICE_VERSION="$TIP"
  else
    export SCALATTICE_VERSION="$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')"
  fi
fi
echo "==> version stamp: ${SCALATTICE_VERSION}"

configure_ws_target

if [[ -z "${CUDACXX:-}" && -x /usr/local/cuda/bin/nvcc ]]; then
  export CUDACXX=/usr/local/cuda/bin/nvcc
fi

if [[ "$SKIP_BUILD" -eq 0 ]]; then
  echo "==> cargo build --release --features gpu"
  cargo build --release --features gpu
else
  echo "==> skipping build (--skip-build)"
fi

if [[ ! -x "$SRC" ]]; then
  echo "Missing $SRC — build first (drop --skip-build)." >&2
  exit 1
fi

mkdir -p "$(dirname "$DEST")"

# Count parent agent runtimes (exclude workers and this script's shell helpers).
count_agent_runtimes() {
  # Match the binary argv0, not repo paths like .../scalattice-agent/scripts/...
  # Parents: foreground / bare binary. Workers have "worker" in argv.
  ps -eo pid=,args= 2>/dev/null \
    | awk '
      /\/scalattice-agent( |$)/ && $0 !~ /awk/ && $0 !~ / worker( |$)/ && $0 !~ /dev-swap-linux/ {
        c++
      }
      END { print c+0 }
    '
}

kill_agent_tree() {
  local sig="${1:--TERM}"
  # Legacy kids + named slot hardlinks (cuda/vulkan/cpu) + supervisor.
  pkill "$sig" -f 'scalattice-agent worker' 2>/dev/null || true
  pkill "$sig" -f 'scalattice-slot-' 2>/dev/null || true
  pkill "$sig" -x scalattice-agent 2>/dev/null || true
  pkill "$sig" -x scalattice-slot-cuda-0 2>/dev/null || true
  pkill "$sig" -x scalattice-slot-cuda-1 2>/dev/null || true
  if [[ -x "$DEST" ]]; then
    pkill "$sig" -f "${DEST}( |$)" 2>/dev/null || true
  fi
  local bin_dir
  bin_dir="$(dirname "$DEST")"
  if [[ -d "$bin_dir" ]]; then
    # Kill by exact hardlink basename if still running.
    local link
    for link in "$bin_dir"/scalattice-slot-*; do
      [[ -e "$link" ]] || continue
      pkill "$sig" -f "$(basename "$link")" 2>/dev/null || true
    done
  fi
}

stop_all_agents() {
  echo "==> stopping every agent runtime (systemd + strays) so only one can own the GPUs"
  if command -v systemctl >/dev/null 2>&1; then
    systemctl --user stop "$WATCHDOG_TIMER" 2>/dev/null || true
    systemctl --user stop "$UNIT" 2>/dev/null || true
  fi
  kill_agent_tree -TERM
  # Drop stale flock holder pid file; next start recreates it.
  rm -f "${CFG_DIR}/agent.runtime.lock"
  local i
  for i in 1 2 3 4 5 6 7 8 9 10; do
    local n
    n="$(count_agent_runtimes)"
    local slots
    # pipefail: pgrep exits 1 when nothing matches — swallow that.
    slots="$(pgrep -f 'scalattice-slot-' 2>/dev/null | wc -l | tr -d ' ' || true)"
    slots="${slots:-0}"
    if [[ "$n" -eq 0 && "$slots" -eq 0 ]]; then
      break
    fi
    echo "    waiting for agent exit… (parents=$n slots=$slots)"
    kill_agent_tree -KILL
    sleep 0.4
  done
  if [[ -e "$DEST" ]] && command -v fuser >/dev/null 2>&1; then
    fuser "$DEST" >/dev/null 2>&1 && fuser -k "$DEST" 2>/dev/null || true
    sleep 0.3
  fi
  local left
  left="$(count_agent_runtimes)"
  if [[ "$left" -ne 0 ]]; then
    echo "error: still $left scalattice-agent process(es) after stop; refuse to swap" >&2
    ps -eo pid,ppid,comm,args= | awk '/scalattice/ && $0 !~ /awk/' >&2 || true
    exit 1
  fi
}

assert_single_agent_runtime() {
  local n
  n="$(count_agent_runtimes)"
  if [[ "$n" -ne 1 ]]; then
    echo "error: expected exactly 1 agent runtime after restart, found $n" >&2
    ps -eo pid,ppid,args= | awk '/scalattice-agent/ && $0 !~ /awk/' >&2 || true
    echo "hint: do not also run a second GPU owner. \`scalattice-agent foreground\` with" >&2
    echo "      systemd installed only follows logs; if you see two owners, stop strays." >&2
    exit 1
  fi
  echo "==> single agent runtime OK (pid $(systemctl --user show -p MainPID --value "$UNIT" 2>/dev/null || echo '?'))"
}

stop_all_agents

echo "==> install $SRC -> $DEST"
cp -f "$SRC" "$DEST"
chmod +x "$DEST"
# Drop stale role hardlinks (old inode) so the new binary recreates them.
rm -f "$(dirname "$DEST")"/scalattice-slot-* "$(dirname "$DEST")"/scalattice-tray 2>/dev/null || true

echo "==> restart (systemd only — never leave a second foreground owner)"
if command -v systemctl >/dev/null 2>&1 && systemctl --user cat "$UNIT" >/dev/null 2>&1; then
  systemctl --user daemon-reload 2>/dev/null || true
  systemctl --user start "$WATCHDOG_TIMER" 2>/dev/null || true
  systemctl --user restart "$UNIT"
  # Give MainPID a moment to appear before the single-instance check.
  sleep 1
  systemctl --user --no-pager --full status "$UNIT" | head -20 || true
  assert_single_agent_runtime
elif command -v scalattice-agent >/dev/null 2>&1; then
  scalattice-agent restart
  sleep 1
  assert_single_agent_runtime
else
  echo "No systemd unit found; start manually: $DEST foreground" >&2
  echo "(only one foreground — do not start a second copy)" >&2
  exit 1
fi

echo "==> done."
"$DEST" --version 2>/dev/null || true
if ! "$DEST" --version 2>/dev/null | grep -qF "${SCALATTICE_VERSION}"; then
  echo "warning: binary version does not match stamp ${SCALATTICE_VERSION}" >&2
fi
if [[ "$CLOUD_MODE" -eq 0 ]]; then
  echo "==> agent WS target: loopback (${SCALATTICE_WS_URL:-$LOCAL_WS_DEFAULT})"
  echo "    Use --cloud to restore the built-in cloud endpoint."
else
  echo "==> agent WS target: cloud default"
fi
echo "==> tip: \`scalattice-agent foreground\` with systemd installed only follows logs;"
echo "    it does not start a second GPU agent. Use journalctl --user -u $UNIT -f to watch."
echo "==> process names (Task Manager equivalent):"
echo "    ps -eo pid,ppid,comm,args | grep -E 'scalattice|PID'"
echo "    htop   # F4 filter: scalattice   (COMM column = short name)"
echo "    pstree -ap | grep -A5 scalattice"
# Quick log peek for register/connect
if command -v journalctl >/dev/null 2>&1; then
  sleep 2
  echo "==> recent agent log:"
  journalctl --user -u "$UNIT" -n 15 --no-pager 2>/dev/null | tail -15 || true
fi
echo "==> after swap process tree:"
ps -eo pid,ppid,comm,args= | awk 'NR==1 || (/scalattice/ && $0 !~ /awk/)' || true
ls -la "$(dirname "$DEST")"/scalattice-slot-* 2>/dev/null || echo "(no slot hardlinks yet — check again after workers spawn)"
