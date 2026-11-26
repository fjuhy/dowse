#!/usr/bin/env bash
# Local merge gate for the fjuhy fork, run on macOS instead of GitHub Actions (Actions is off on the fork).
# Mirrors .github/workflows/ci.yml as far as a Mac can, and lints #[cfg(windows)] code with a check-only
# x86_64-pc-windows-msvc clippy (zig compiles C deps, nothing is linked or run).
# Usage: dev/check.sh [step...]   steps: fmt clippy wincheck test nodefault doc frontend (default: all)
# Result: per-step pass/FAIL, saved with HEAD and time to .check/last.txt (gitignored). Exit 1 if any step fails.
# Env:
#   DOWSE_RUST=1.89.0     toolchain for builds (File::try_lock needs 1.89; stable 1.9x breaks tauri-build 2.6.3's sidecar copy)
#   DOWSE_ZIG_PY=<python> python with the `ziglang` package (default ~/hermes-jobs/dowse7-wincheck/zigenv/bin/python)
#   DOWSE_CHECK_FRESH=1   always run npm ci
# npm audit and cargo deny are printed for information and never block.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/.check"
APP="$ROOT/crates/dowse-app"
mkdir -p "$OUT"
STEPS=("$@")
[ ${#STEPS[@]} -eq 0 ] && STEPS=(fmt clippy wincheck test nodefault doc frontend)
export RUSTUP_TOOLCHAIN="${DOWSE_RUST:-1.89.0}"
export DOWSE_ZIG_PY="${DOWSE_ZIG_PY:-$HOME/hermes-jobs/dowse7-wincheck/zigenv/bin/python}"
HOST="$(rustc -vV | sed -n 's/^host: //p')"

# tauri-build needs the externalBin sidecar to exist at compile time (ci.yml copies a debug build too).
sidecar() {
  local bin="$APP/src-tauri/binaries/dowse-$HOST"
  [ -f "$bin" ] && return 0
  (cd "$ROOT" && cargo build -q -p dowse) || return 1
  mkdir -p "$APP/src-tauri/binaries" && cp "${CARGO_TARGET_DIR:-$ROOT/target}/debug/dowse" "$bin"
}

step_fmt() { cd "$ROOT" && cargo fmt --check; }
# On macOS the Windows-only code paths have no callers, so unused-code lints are left to the wincheck step,
# which runs ci.yml's strict clippy against the Windows target.
step_clippy() { sidecar && cd "$ROOT" && cargo clippy --workspace --all-targets -- -D warnings -A dead_code -A unused_variables -A unused_imports; }
step_wincheck() {
  rustup target list --installed | grep -qx x86_64-pc-windows-msvc \
    || { echo "missing: rustup target add x86_64-pc-windows-msvc --toolchain $RUSTUP_TOOLCHAIN"; return 1; }
  "$DOWSE_ZIG_PY" -c "import ziglang" 2>/dev/null \
    || { echo "missing: a python with ziglang (pip install ziglang), set DOWSE_ZIG_PY"; return 1; }
  local exe="$APP/src-tauri/binaries/dowse-x86_64-pc-windows-msvc.exe"
  [ -e "$exe" ] || { mkdir -p "$(dirname "$exe")" && : > "$exe"; }
  local W="$ROOT/dev/wincheck"
  cd "$ROOT" && CC_x86_64_pc_windows_msvc="$W/zcc" CXX_x86_64_pc_windows_msvc="$W/zcc" \
    AR_x86_64_pc_windows_msvc="$W/zar" RC_x86_64_pc_windows_msvc="$W/stub-rc" \
    CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}/wincheck" \
    cargo clippy -p dowse -p dowse-app --all-targets --target x86_64-pc-windows-msvc -- -D warnings
}
step_test() { sidecar && cd "$ROOT" && cargo test --workspace; }
step_nodefault() { cd "$ROOT" && cargo check -p dowse --no-default-features; }
step_doc() { cd "$ROOT" && cargo doc -p dowse --no-deps; }
step_frontend() {
  cd "$APP" || return 1
  local sum stamp="$APP/node_modules/.dowse-check-lock"
  sum="$(shasum package-lock.json | cut -d' ' -f1)"
  if [ "${DOWSE_CHECK_FRESH:-0}" = 1 ] || [ ! -f "$stamp" ] || [ "$(cat "$stamp")" != "$sum" ]; then
    { npm ci --no-audit --no-fund && echo "$sum" > "$stamp"; } || return 1
  fi
  npm audit --audit-level=high || echo "info: npm audit reported advisories (not blocking)"
  npm run test:unit && npm run check && npm run build && npm run test:ui
}

HEAD="$(git -C "$ROOT" rev-parse --short HEAD)"
DIRTY="$(git -C "$ROOT" status --porcelain --untracked-files=no | grep -q . && echo ' (uncommitted changes)' || true)"
START="$(date '+%Y-%m-%d %H:%M:%S %Z')"
SUMMARY=(); FAIL=0
for s in "${STEPS[@]}"; do
  if ! declare -F "step_$s" >/dev/null; then echo "unknown step: $s" >&2; exit 2; fi
  echo "=== $s ==="
  t0=$SECONDS
  if ( "step_$s" ) >"$OUT/$s.log" 2>&1; then
    SUMMARY+=("pass  $s ($((SECONDS - t0))s)")
  else
    SUMMARY+=("FAIL  $s ($((SECONDS - t0))s) -> .check/$s.log")
    tail -20 "$OUT/$s.log"
    FAIL=1
  fi
done
if command -v cargo-deny >/dev/null; then (cd "$ROOT" && cargo deny check advisories >"$OUT/deny.log" 2>&1) || echo "info: cargo deny advisories found (not blocking) -> .check/deny.log"; fi
{
  echo "dowse check ($RUSTUP_TOOLCHAIN) $HEAD$DIRTY $START -> $(date '+%H:%M:%S')"
  printf '%s\n' "${SUMMARY[@]}"
  [ $FAIL -eq 0 ] && echo "result: pass" || echo "result: FAIL"
} | tee "$OUT/last.txt"
exit $FAIL
