#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0
FAIL=0
. "$HERE/../test-assertions.sh" || exit 1
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null | head -20; FAIL=$((FAIL + 1)); fi
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
  git -C "$REPO" init -q -b main
  git -C "$REPO" -c user.name=t -c user.email=t@t commit -q --allow-empty -m init
  cp "$HERE/disk-gc.sh" "$AGM_DIR/bin/disk-gc.sh"
  LOG="$AGM_DIR/disk-gc.log"; : > "$LOG"
  RC="$HOME/.cache/agents-manager/remote-cargo"
  : > "$FIX/agm.log"
  cat > "$BIN/agm" <<STUB
#!/bin/bash
echo "agm \$*" >> "$FIX/agm.log"
exit 0
STUB
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
if touch -d '30 days ago' "$(mktemp)" 2>/dev/null; then
  old() { touch -d '30 days ago' "$@"; }
else
  old() { touch -t "$(date -v-30d +%Y%m%d%H%M)" "$@"; }
fi
mk_target() {
  local t="$1"
  mkdir -p "$t/target/debug/incremental/oldcrate-aaa" "$t/target/debug/incremental/newcrate-bbb" "$t/target/debug/.fingerprint"
  echo x > "$t/target/debug/incremental/oldcrate-aaa/f"; echo x > "$t/target/debug/incremental/newcrate-bbb/f"
  old "$t/target/debug/incremental/oldcrate-aaa"
  if [ "${2:-no}" = yes ]; then old "$t/target/debug/.fingerprint"; fi
}
run() { PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" AGM_REPO="$REPO" DF_BIN="$BIN/df" AGM_BIN="$BIN/agm" DISK_GC_LOG="$LOG" bash "$AGM_DIR/bin/disk-gc.sh" >/dev/null 2>&1; echo $?; }

if [ ! -r /proc/self/cwd ] && [ ! -L /proc/self/cwd ]; then
  echo "skip - 此環境沒有 /proc：刪除類案例略過（disk-gc.sh 在這裡不刪任何東西），只驗拒絕行為"
  setup
  mkdir -p "$RC/oldproj/x"; echo x > "$RC/oldproj/x/f"; old "$RC/oldproj"
  mk_target "$REPO" yes
  equals "沒有 /proc：exit 0" "$(run)" "0"
  exists "沒有 /proc：舊的 remote-cargo 目錄不刪" "$RC/oldproj"
  exists "沒有 /proc：主樹舊的 incremental 不刪" "$REPO/target/debug/incremental/oldcrate-aaa"
  check  "沒有 /proc：log 記下原因" "沒有 /proc" "$LOG"
  teardown
  echo "$PASS passed, $FAIL failed"
  [ "$FAIL" -eq 0 ]
  exit $?
fi

setup
mkdir -p "$RC/oldproj/x" "$RC/newproj/x"; echo x > "$RC/oldproj/x/f"; old "$RC/oldproj"
equals "正常跑 exit 0" "$(run)" "0"
gone   "舊的 remote-cargo 目錄被刪" "$RC/oldproj"
exists "新的 remote-cargo 目錄留著" "$RC/newproj"
teardown

setup
WT="$REPO/.claude/worktrees/wt1"; mkdir -p "$WT"; mk_target "$WT"
mk_target "$REPO" yes
equals "exit 0" "$(run)" "0"
gone   "worktree 舊的 incremental 被刪" "$WT/target/debug/incremental/oldcrate-aaa"
exists "worktree 新的 incremental 留著" "$WT/target/debug/incremental/newcrate-bbb"
gone   "主樹舊的 incremental 被刪" "$REPO/target/debug/incremental/oldcrate-aaa"
exists "主樹新的 incremental 留著" "$REPO/target/debug/incremental/newcrate-bbb"
exists "主樹的 target/ 永不整個刪" "$REPO/target"
teardown

setup
OLDWT="$REPO/.claude/worktrees/stale"; NEWWT="$REPO/.claude/worktrees/active"; mkdir -p "$OLDWT" "$NEWWT"
mk_target "$OLDWT" yes; mk_target "$NEWWT"
echo src > "$OLDWT/keep.rs"
equals "exit 0" "$(run)" "0"
gone   "久沒用的 worktree 整個 target/ 被刪" "$OLDWT/target"
exists "worktree 的原始碼不碰" "$OLDWT/keep.rs"
exists "最近有編譯的 worktree target/ 留著" "$NEWWT/target"
teardown

setup
REBUILT="$REPO/.claude/worktrees/rebuilt"; mkdir -p "$REBUILT"; mk_target "$REBUILT"
mkdir -p "$REBUILT/target/debug/.fingerprint/foo-abc"
echo x > "$REBUILT/target/debug/.fingerprint/foo-abc/invoked.timestamp"
old "$REBUILT/target/debug/.fingerprint/foo-abc" "$REBUILT/target/debug/.fingerprint"
equals "exit 0" "$(run)" "0"
exists "最近重編過既有 unit 的 worktree：target/ 留著" "$REBUILT/target"
teardown

setup
STALE="$REPO/.claude/worktrees/stale-unit"; mkdir -p "$STALE"; mk_target "$STALE"
mkdir -p "$STALE/target/debug/.fingerprint/foo-abc"
echo x > "$STALE/target/debug/.fingerprint/foo-abc/invoked.timestamp"
old "$STALE/target/debug/.fingerprint/foo-abc/invoked.timestamp" "$STALE/target/debug/.fingerprint/foo-abc" "$STALE/target/debug/.fingerprint"
equals "exit 0" "$(run)" "0"
gone "unit 與檔都很舊的 worktree：target/ 整個刪" "$STALE/target"
teardown

setup
UNREAD="$REPO/.claude/worktrees/unreadable"; mkdir -p "$UNREAD"; mk_target "$UNREAD" yes
mkdir -p "$UNREAD/target/debug/.fingerprint/bar-xyz"
echo x > "$UNREAD/target/debug/.fingerprint/bar-xyz/invoked.timestamp"
old "$UNREAD/target/debug/.fingerprint/bar-xyz" "$UNREAD/target/debug/.fingerprint"
echo x > "$UNREAD/target/keep-me"; old "$UNREAD/target/keep-me"
chmod 000 "$UNREAD/target/debug/.fingerprint/bar-xyz"
if [ -r "$UNREAD/target/debug/.fingerprint/bar-xyz" ]; then
  echo "FAIL - 無法模擬讀不到（以 root 跑？）：這條測試需要非 root 身份"; FAIL=$((FAIL + 1))
fi
equals "exit 0" "$(run)" "0"
exists "讀不完的 unit 子樹：target/ 整棵保留，哨兵也還在（不因為 find 沒輸出就當成久沒用）" "$UNREAD/target/keep-me"
check  "log 記下讀不完的原因" "讀不完" "$LOG"
chmod 755 "$UNREAD/target/debug/.fingerprint/bar-xyz" 2>/dev/null
teardown

setup
BUSY="$REPO/.claude/worktrees/busy"; mkdir -p "$BUSY"; mk_target "$BUSY" yes
( cd "$BUSY" && exec sleep 30 ) >/dev/null 2>&1 & SLEEPERS="$SLEEPERS $!"
sleep 0.5
equals "exit 0" "$(run)" "0"
exists "有行程在用的 worktree：target/ 不動" "$BUSY/target"
exists "有行程在用的 worktree：舊 incremental 也不動" "$BUSY/target/debug/incremental/oldcrate-aaa"
check  "log 記下跳過的原因" "有行程的 cwd 在底下" "$LOG"
teardown

setup
mkdir -p "$ROOT/elsewhere/oldproj"; old "$ROOT/elsewhere/oldproj"
ln -s "$ROOT/elsewhere" "$RC"
equals "exit 0" "$(run)" "0"
exists "symlink 指到的目錄裡的東西沒被刪" "$ROOT/elsewhere/oldproj"
exists "symlink 本身還在" "$RC"
check  "log 記下拒絕" "remote-cargo（路徑不符預期" "$LOG"
teardown

setup
mkdir -p "$ROOT/outside"; mk_target "$ROOT/outside" yes
ln -s "$ROOT/outside" "$REPO/.claude/worktrees/linked"
equals "exit 0" "$(run)" "0"
exists "symlink 的 worktree 不碰" "$ROOT/outside/target"
teardown

setup
mkdir -p "$ROOT/home/x"; mk_target "$REPO" yes
PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" AGM_REPO="$REPO/../project/agents-manager" DF_BIN="$BIN/df" AGM_BIN="$BIN/agm" DISK_GC_LOG="$LOG" bash "$AGM_DIR/bin/disk-gc.sh" >/dev/null 2>&1
exists "AGM_REPO 帶 .. 時不刪" "$REPO/target/debug/incremental/oldcrate-aaa"
teardown

setup
REG="$HOME/project/agm-x"; git -C "$REPO" worktree add -q "$REG" -b agm-x
mk_target "$REG"
equals "exit 0" "$(run)" "0"
gone   "登記的 worktree（非 .claude）舊的 incremental 被刪" "$REG/target/debug/incremental/oldcrate-aaa"
exists "登記的 worktree（非 .claude）新的 incremental 留著" "$REG/target/debug/incremental/newcrate-bbb"
teardown

setup
REGOLD="$HOME/project/agm-stale"; git -C "$REPO" worktree add -q "$REGOLD" -b agm-stale
mk_target "$REGOLD" yes
equals "exit 0" "$(run)" "0"
gone   "登記的 worktree（非 .claude）.fingerprint 全舊：整個 target/ 刪" "$REGOLD/target"
teardown

setup
BUSYREG="$HOME/project/agm-busy"; git -C "$REPO" worktree add -q "$BUSYREG" -b agm-busy
mk_target "$BUSYREG" yes
( cd "$BUSYREG" && exec sleep 30 ) >/dev/null 2>&1 & SLEEPERS="$SLEEPERS $!"
BUSYPID=$!
for _ in $(seq 1 2000); do [ "$(readlink "/proc/$BUSYPID/cwd" 2>/dev/null)" = "$BUSYREG" ] && break; done
equals "exit 0" "$(run)" "0"
exists "登記的 worktree 有行程在用：target/ 不動" "$BUSYREG/target"
check  "log 記下跳過的原因" "有行程的 cwd 在底下" "$LOG"
teardown

setup
OUTREG="$ROOT/outside-home-wt"; git -C "$REPO" worktree add -q "$OUTREG" -b outside
mk_target "$OUTREG" yes
equals "exit 0" "$(run)" "0"
exists "登記在 HOME 之外的 worktree：target/ 不動" "$OUTREG/target/debug/incremental/oldcrate-aaa"
teardown

setup
CIREPO="$HOME/.cache/agents-manager/ci/repo"; mkdir -p "$CIREPO"
mk_target "$CIREPO" yes
equals "exit 0" "$(run)" "0"
gone   "CI clone 舊的 incremental 被刪" "$CIREPO/target/debug/incremental/oldcrate-aaa"
exists "CI clone 新的 incremental 留著" "$CIREPO/target/debug/incremental/newcrate-bbb"
exists "CI clone 的 target/ 整個不刪（即使 .fingerprint 全舊）" "$CIREPO/target"
teardown

setup
cat > "$BIN/git-fail" <<'STUB'
#!/bin/bash
exit 1
STUB
chmod 755 "$BIN/git-fail"
WT2="$REPO/.claude/worktrees/wt-glob"; mkdir -p "$WT2"; mk_target "$WT2"
equals "exit 0" "$(PATH="$BIN:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" DISK_GC_GIT="$BIN/git-fail" AGM_REPO="$REPO" DF_BIN="$BIN/df" AGM_BIN="$BIN/agm" DISK_GC_LOG="$LOG" bash "$AGM_DIR/bin/disk-gc.sh" >/dev/null 2>&1; echo $?)" "0"
gone   "git 失敗時 .claude/worktrees 照舊掃" "$WT2/target/debug/incremental/oldcrate-aaa"
check  "log 記下 worktree list 失敗" "worktree list 失敗" "$LOG"
teardown

age_tree() {
  local f
  while IFS= read -r f; do old "$f"; done < <(find "$1")
}
setup
POOL="$RC/hashA/shared/target/debug"
mkdir -p "$POOL/incremental/oldcrate-aaa/s1" "$POOL/incremental/newcrate-bbb" "$POOL/incremental/mixed-ccc/s1" "$POOL/deps" "$RC/hashA/shared/target/release/incremental/oldrel-ddd"
echo x > "$POOL/incremental/oldcrate-aaa/s1/f"; echo x > "$POOL/incremental/newcrate-bbb/f"; echo x > "$POOL/incremental/mixed-ccc/s1/f"; echo x > "$POOL/deps/libold.rlib"
echo x > "$RC/hashA/shared/target/release/incremental/oldrel-ddd/f"
age_tree "$POOL/incremental/oldcrate-aaa"; age_tree "$POOL/deps"; age_tree "$RC/hashA/shared/target/release/incremental/oldrel-ddd"
age_tree "$POOL/incremental/mixed-ccc"; echo y > "$POOL/incremental/mixed-ccc/s1/fresh"
mkdir -p "$ROOT/outside-crate"; echo x > "$ROOT/outside-crate/f"; age_tree "$ROOT/outside-crate"
ln -s "$ROOT/outside-crate" "$POOL/incremental/linkcrate-eee"
equals "exit 0" "$(run)" "0"
gone   "熱池：整棵舊的 crate 目錄被刪" "$POOL/incremental/oldcrate-aaa"
exists "熱池：新的 crate 目錄留著" "$POOL/incremental/newcrate-bbb"
exists "熱池：裡面有近期檔的 crate 目錄留著（目錄本身舊）" "$POOL/incremental/mixed-ccc"
exists "熱池：deps 不刪" "$POOL/deps/libold.rlib"
gone   "熱池：release profile 的舊 crate 也刪" "$RC/hashA/shared/target/release/incremental/oldrel-ddd"
exists "熱池：shared 與 target 本身留著" "$POOL"
exists "熱池：symlink crate 本身留著" "$POOL/incremental/linkcrate-eee"
exists "熱池：symlink 指到的目錄沒被刪" "$ROOT/outside-crate/f"
teardown

setup
POOL="$RC/hashB/shared/target/debug"
mkdir -p "$POOL/incremental/busycrate-fff" "$POOL/incremental/idle-ggg"
echo x > "$POOL/incremental/busycrate-fff/f"; echo x > "$POOL/incremental/idle-ggg/f"
age_tree "$POOL/incremental/busycrate-fff"; age_tree "$POOL/incremental/idle-ggg"
( cd "$POOL/incremental/busycrate-fff" && exec sleep 30 ) >/dev/null 2>&1 & SLEEPERS="$SLEEPERS $!"
BUSYPOOL=$!
for _ in $(seq 1 2000); do [ "$(readlink "/proc/$BUSYPOOL/cwd" 2>/dev/null)" = "$(cd "$POOL/incremental/busycrate-fff" && pwd -P)" ] && break; done
equals "exit 0" "$(run)" "0"
exists "熱池：有行程在用的舊 crate 目錄不動" "$POOL/incremental/busycrate-fff/f"
gone   "熱池：沒人用的舊 crate 目錄照刪" "$POOL/incremental/idle-ggg"
check  "熱池：log 記下跳過的原因" "有行程在用" "$LOG"
teardown

setup
mkdir -p "$ROOT/elsewhere-pool/target/debug/incremental/oldx-hhh"; echo x > "$ROOT/elsewhere-pool/target/debug/incremental/oldx-hhh/f"
age_tree "$ROOT/elsewhere-pool"
mkdir -p "$RC/hashC"; ln -s "$ROOT/elsewhere-pool" "$RC/hashC/shared"
equals "exit 0" "$(run)" "0"
exists "熱池 shared 是 symlink：指到的目錄不被刪" "$ROOT/elsewhere-pool/target/debug/incremental/oldx-hhh/f"
check  "熱池 shared 是 symlink：log 記下拒絕" "路徑不符預期" "$LOG"
teardown

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
