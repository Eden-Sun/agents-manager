#!/usr/bin/env bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/verify-embedded-ui.sh"
PASS=0
FAIL=0
ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT

ok() { echo "ok   - $1"; PASS=$((PASS + 1)); }
bad() { echo "FAIL - $1"; FAIL=$((FAIL + 1)); }
expect_exit() {
  local name="$1" want="$2"; shift 2
  local out rc
  out="$("$@" 2>&1)"; rc=$?
  if { [ "$want" = 0 ] && [ "$rc" = 0 ]; } || { [ "$want" != 0 ] && [ "$rc" != 0 ]; }; then ok "$name"
  else bad "${name}（exit ${rc}，預期 ${want}）：${out}"; fi
}

[ -f "$SCRIPT" ] && ok "腳本存在" || bad "腳本存在（${SCRIPT}）"

DIST="$ROOT/dist"
mkdir -p "$DIST/assets"
printf '<script type="module" src="/assets/app-NEW.js"></script>\n' > "$DIST/index.html"
printf 'console.log("new ui");' > "$DIST/assets/app-NEW.js"

GOOD="$ROOT/good.bin"
{ printf 'ELF-junk'; cat "$DIST/index.html"; cat "$DIST/assets/app-NEW.js"; printf 'tail'; } > "$GOOD"
expect_exit "前端一致：通過" 0 bash "$SCRIPT" "$GOOD" "$DIST"

STALE="$ROOT/stale.bin"
{ printf 'ELF-junk'; printf '<script type="module" src="/assets/app-OLD.js"></script>\n'; printf 'console.log("old ui");'; } > "$STALE"
expect_exit "binary 是舊前端：不通過（#1071 的主要症狀）" 1 bash "$SCRIPT" "$STALE" "$DIST"

HALF="$ROOT/half.bin"
{ printf 'ELF-junk'; cat "$DIST/index.html"; printf 'console.log("old ui");'; } > "$HALF"
expect_exit "asset 內容不在 binary 裡：不通過" 1 bash "$SCRIPT" "$HALF" "$DIST"

EMPTY="$ROOT/empty"; mkdir -p "$EMPTY"
expect_exit "web/dist 沒有 index.html：不通過" 1 bash "$SCRIPT" "$GOOD" "$EMPTY"

expect_exit "binary 不存在：不通過" 1 bash "$SCRIPT" "$ROOT/nope.bin" "$DIST"

MISS="$ROOT/miss"; mkdir -p "$MISS"
cp "$DIST/index.html" "$MISS/index.html"
expect_exit "dist 缺 asset 檔：不通過" 1 bash "$SCRIPT" "$GOOD" "$MISS"

echo "verify-embedded-ui: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
