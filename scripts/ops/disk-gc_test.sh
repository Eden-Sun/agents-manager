#!/bin/bash
# disk-gc.sh 的隔離測試（issue #835）：HOME 指到暫存目錄，腳本原樣複製進去跑；假 df／agm；
# 用 touch -d 造舊檔。破壞性那一條：只刪明確路徑、主樹的 target/ 永不整個刪、有行程在用就不動、symlink 一律拒絕。
#
#   bash scripts/ops/disk-gc_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0
FAIL=0
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null | head -20; FAIL=$((FAIL + 1)); fi
}
equals() {
  if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（是 '$2'，預期 '$3'）"; FAIL=$((FAIL + 1)); fi
}
exists() { [ -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 不見了）"; FAIL=$((FAIL + 1)); }; }
gone() { [ ! -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 還在）"; FAIL=$((FAIL + 1)); }; }

SLEEPERS=""
setup() {
  ROOT="$(mktemp -d)"
  export HOME="$ROOT/home"
  AGM_DIR="$HOME/.config/agents-manager/supervisor/AGM"
  FIX="$ROOT/fix"; BIN="$ROOT/fakebin"
  REPO="$HOME/project/agents-manager"
  mkdir -p "$AGM_DIR/bin" "$HOME/.cache/agents-manager" "$REPO/target/debug" "$REPO/.claude/worktrees" "$FIX" "$BIN"
  cp "$HERE/disk-gc.sh" "$AGM_DIR/bin/disk-gc.sh"
  LOG="$AGM_DIR/disk-gc.log"; : > "$LOG"
  RC="$HOME/.cache/agents-manager/remote-cargo"
  : > "$FIX/agm.log"
  cat > "$BIN/agm" <<STUB
#!/bin/bash
echo "agm \$*" >> "$FIX/agm.log"
exit 0
STUB
  # 假 df：FIX/pct 的百分比（預設 50），剩餘 500 GB。
  cat > "$BIN/df" <<STUB
#!/bin/bash
pct=\$(cat "$FIX/pct" 2>/dev/null || echo 50)
echo "Filesystem 1024-blocks Used Available Capacity Mounted"
echo "/dev/fake 1000000000 500000000 524288000 \${pct}% /"
STUB
  chmod 755 "$BIN"/*
}
teardown() {
  for p in $SLEEPERS; do kill "$p" 2>/dev/null; done; SLEEPERS=""
  rm -rf "$ROOT"
}
old() { touch -d '30 days ago' "$@"; }
# 一棵有 target/debug/incremental/{old,new} 與 .fingerprint 的樹
mk_target() { # mk_target <樹> [舊 fingerprint?yes]
  local t="$1"
  mkdir -p "$t/target/debug/incremental/oldcrate-aaa" "$t/target/debug/incremental/newcrate-bbb" "$t/target/debug/.fingerprint"
  echo x > "$t/target/debug/incremental/oldcrate-aaa/f"; echo x > "$t/target/debug/incremental/newcrate-bbb/f"
  old "$t/target/debug/incremental/oldcrate-aaa"
  if [ "${2:-no}" = yes ]; then old "$t/target/debug/.fingerprint"; fi
}
run() { PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" AGM_REPO="$REPO" DF_BIN="$BIN/df" AGM_BIN="$BIN/agm" DISK_GC_LOG="$LOG" bash "$AGM_DIR/bin/disk-gc.sh" >/dev/null 2>&1; echo $?; }

# 1. remote-cargo：舊的刪、新的留。
setup
mkdir -p "$RC/oldproj/x" "$RC/newproj/x"; echo x > "$RC/oldproj/x/f"; old "$RC/oldproj"
equals "正常跑 exit 0" "$(run)" "0"
gone   "舊的 remote-cargo 目錄被刪" "$RC/oldproj"
exists "新的 remote-cargo 目錄留著" "$RC/newproj"
teardown

# 2. worktree 舊的 incremental 刪、新的留；主樹的 incremental 同規則，但主樹整個 target/ 永不刪。
setup
WT="$REPO/.claude/worktrees/wt1"; mkdir -p "$WT"; mk_target "$WT"
mk_target "$REPO" yes     # 主樹的 .fingerprint 很舊：worktree 會整個刪，主樹不會
equals "exit 0" "$(run)" "0"
gone   "worktree 舊的 incremental 被刪" "$WT/target/debug/incremental/oldcrate-aaa"
exists "worktree 新的 incremental 留著" "$WT/target/debug/incremental/newcrate-bbb"
gone   "主樹舊的 incremental 被刪" "$REPO/target/debug/incremental/oldcrate-aaa"
exists "主樹新的 incremental 留著" "$REPO/target/debug/incremental/newcrate-bbb"
exists "主樹的 target/ 永不整個刪" "$REPO/target"
teardown

# 3. worktree 的 .fingerprint 都很舊 → 整個 target/ 刪；.fingerprint 新 → 留。
setup
OLDWT="$REPO/.claude/worktrees/stale"; NEWWT="$REPO/.claude/worktrees/active"; mkdir -p "$OLDWT" "$NEWWT"
mk_target "$OLDWT" yes; mk_target "$NEWWT"
echo src > "$OLDWT/keep.rs"
equals "exit 0" "$(run)" "0"
gone   "久沒用的 worktree 整個 target/ 被刪" "$OLDWT/target"
exists "worktree 的原始碼不碰" "$OLDWT/keep.rs"
exists "最近有編譯的 worktree target/ 留著" "$NEWWT/target"
teardown

# 4. 有行程的 cwd 在某 worktree → 那棵完全不動（即使 target 很舊）。
setup
BUSY="$REPO/.claude/worktrees/busy"; mkdir -p "$BUSY"; mk_target "$BUSY" yes
( cd "$BUSY" && exec sleep 30 ) >/dev/null 2>&1 & SLEEPERS="$SLEEPERS $!"
sleep 0.5
equals "exit 0" "$(run)" "0"
exists "有行程在用的 worktree：target/ 不動" "$BUSY/target"
exists "有行程在用的 worktree：舊 incremental 也不動" "$BUSY/target/debug/incremental/oldcrate-aaa"
check  "log 記下跳過的原因" "有行程的 cwd 在底下" "$LOG"
teardown

# 5. remote-cargo 是 symlink 指到別處 → 拒絕、不刪任何東西。
setup
mkdir -p "$ROOT/elsewhere/oldproj"; old "$ROOT/elsewhere/oldproj"
ln -s "$ROOT/elsewhere" "$RC"
equals "exit 0" "$(run)" "0"
exists "symlink 指到的目錄裡的東西沒被刪" "$ROOT/elsewhere/oldproj"
exists "symlink 本身還在" "$RC"
check  "log 記下拒絕" "remote-cargo（路徑不符預期" "$LOG"
teardown

# 5b. worktrees 目錄或某個 worktree 是 symlink → 不跟。
setup
mkdir -p "$ROOT/outside"; mk_target "$ROOT/outside" yes
ln -s "$ROOT/outside" "$REPO/.claude/worktrees/linked"
equals "exit 0" "$(run)" "0"
exists "symlink 的 worktree 不碰" "$ROOT/outside/target"
teardown

# 5c. AGM_REPO 含 `..` → 整步放棄。
setup
mkdir -p "$ROOT/home/x"; mk_target "$REPO" yes
PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" AGM_REPO="$REPO/../project/agents-manager" DF_BIN="$BIN/df" AGM_BIN="$BIN/agm" DISK_GC_LOG="$LOG" bash "$AGM_DIR/bin/disk-gc.sh" >/dev/null 2>&1
exists "AGM_REPO 帶 .. 時不刪" "$REPO/target/debug/incremental/oldcrate-aaa"
teardown

# 6. 磁碟水位：90% → 呼叫 ops-alert 一次；50% → 不呼叫。
setup
echo 90 > "$FIX/pct"
equals "90% exit 0" "$(run)" "0"
equals "90% 呼叫 ops-alert 一次" "$(grep -c 'ops-alert --source disk-gc --reason disk_low' "$FIX/agm.log")" "1"
teardown
setup
echo 50 > "$FIX/pct"
equals "50% exit 0" "$(run)" "0"
equals "50% 不呼叫 agm" "$(wc -c < "$FIX/agm.log" | tr -d ' ')" "0"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
