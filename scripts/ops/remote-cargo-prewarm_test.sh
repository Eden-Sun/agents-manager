#!/bin/bash
# remote-cargo-prewarm.sh 的隔離測試（issue #1855）：假 daemon（記下 remote-cargo 的參數；結束碼由 FAKE_RC 決定，
# FAKE_RC_FILE 的每一行依序給每次呼叫）、假 cargo（被叫到就記 LOCAL-CARGO，代表跑到本機了）、真的 git（本機 bare 的 origin）。
# 不碰 r7、正式 AGM 或 ~/.config。
#
#   bash scripts/ops/remote-cargo-prewarm_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/remote-cargo-prewarm.sh"
UNIT_DIR="$HERE/systemd"
PASS=0
FAIL=0

ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
LOG="$ROOT/calls.log"
WT="$ROOT/wt"
CFG="$ROOT/config.toml"
STATE="$ROOT/state/prewarm.sha"
RETRY="${STATE}.retry"
RC_FILE="$ROOT/rc-seq"

# 假 daemon：只記下 remote-cargo 的參數。FAKE_RC_FILE 有內容就取第一行當結束碼（取完刪掉），否則用 FAKE_RC（沒設就 0）。
mkdir -p "$ROOT/bin" "$ROOT/fakebin"
cat > "$ROOT/bin/agents-managerd" <<'STUB'
#!/bin/bash
# --list-hosts（#1956）：印 FAKE_HOSTS（空白分隔），不記進呼叫記錄。
if [ "${4:-}" = "--list-hosts" ]; then
    for h in ${FAKE_HOSTS:-}; do echo "$h"; done
    exit 0
fi
echo "daemon $*" >> "$LOG"
if [ -n "${FAKE_RC_FILE:-}" ] && [ -s "$FAKE_RC_FILE" ]; then
    rc=$(head -n 1 "$FAKE_RC_FILE")
    sed -i 1d "$FAKE_RC_FILE"
    exit "$rc"
fi
exit "${FAKE_RC:-0}"
STUB
chmod +x "$ROOT/bin/agents-managerd"
# 假 cargo：本機跑到這裡就記下來；預熱絕不該走到這裡。
cat > "$ROOT/fakebin/cargo" <<'STUB'
#!/bin/bash
echo "LOCAL-CARGO $*" >> "$LOG"
exit 0
STUB
chmod +x "$ROOT/fakebin/cargo"

# 本機 bare origin ＋ 工作樹 repo（origin/main 就是預熱的目標）。
git init -q --bare "$ROOT/origin.git"
git init -q -b main "$ROOT/repo"
git -C "$ROOT/repo" remote add origin "$ROOT/origin.git"
echo one > "$ROOT/repo/a.txt"
# 跟真的 repo 一樣：web/.gitignore 忽略 dist（預熱放的 web/dist stub 不會讓 worktree 變髒）。
mkdir -p "$ROOT/repo/web" && echo dist > "$ROOT/repo/web/.gitignore"
git -C "$ROOT/repo" add a.txt web/.gitignore
git -C "$ROOT/repo" commit -q -m one
git -C "$ROOT/repo" push -q origin main
: > "$CFG"

push_new_main() {
    echo "$1" >> "$ROOT/repo/a.txt"
    git -C "$ROOT/repo" commit -qam "$1"
    git -C "$ROOT/repo" push -q origin main
}

reset_state() {
    rm -rf "$ROOT/state" "$LOG" "$RC_FILE"
    : > "$LOG"
    if [ -e "$WT" ]; then
        git -C "$ROOT/repo" worktree remove --force "$WT" 2>/dev/null
        rm -rf "$WT"
    fi
    git -C "$ROOT/repo" worktree prune
}

run() {
    env AGM_PREWARM_REPO="$ROOT/repo" AGM_PREWARM_WORKTREE="$WT" AGM_PREWARM_CONFIG="$CFG" \
        AGM_PREWARM_DAEMON="${DAEMON_OVERRIDE:-$ROOT/bin/agents-managerd}" AGM_PREWARM_STATE="$STATE" \
        AGM_PREWARM_FETCH_TIMEOUT=30 LOG="$LOG" FAKE_RC_FILE="$RC_FILE" PATH="$ROOT/fakebin:$PATH" \
        bash "$SCRIPT" 2>&1
}

equals() {
    local name="$1" got="$2" want="$3"
    if [ "$got" = "$want" ]; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
        echo "FAIL: ${name}"
        echo "  want: ${want}"
        echo "  got:  ${got}"
    fi
}

contains() {
    local name="$1" text="$2" needle="$3"
    case "$text" in
        *"$needle"*) PASS=$((PASS + 1)) ;;
        *) FAIL=$((FAIL + 1)); echo "FAIL: ${name}（找不到 ${needle}）"; echo "$text" ;;
    esac
}

origin_sha() { git -C "$ROOT/repo" rev-parse origin/main; }
count() { grep -c "$1" "$LOG" 2>/dev/null || true; }
exists() { if [ -e "$1" ]; then echo yes; else echo no; fi; }
# 三步（沒有 crates 目錄）：check -p agents-managerd、test -p agents-managerd --no-run、test -p xtask --no-run。
step_call() { echo "daemon remote-cargo --config $CFG --cwd $WT -- $1"; }
CHECK_CALL=$(step_call "check -p agents-managerd --all-targets --locked")
TEST_DM_CALL=$(step_call "test -p agents-managerd --no-run --locked")
TEST_XTASK_CALL=$(step_call "test -p xtask --no-run --locked")
MARKER_NAME="agm-remote-cargo-prewarm"
marker_of() { echo "$(git -C "$WT" rev-parse --path-format=absolute --git-dir)/${MARKER_NAME}"; }

# 1. 第一次：origin/main 還沒預熱 → 在 r7 跑三步，記下 sha，worktree 停在那個 sha。
reset_state
out=$(run); rc=$?
equals "首次預熱 exit 0" "$rc" "0"
equals "首次預熱呼叫 remote-cargo 三次" "$(wc -l < "$LOG" | tr -d ' ')" "3"
equals "首次預熱 check 參數（--cwd 是專用 worktree）" "$(sed -n 1p "$LOG")" "$CHECK_CALL"
equals "首次預熱 daemon test 參數" "$(sed -n 2p "$LOG")" "$TEST_DM_CALL"
equals "首次預熱 xtask test 參數" "$(sed -n 3p "$LOG")" "$TEST_XTASK_CALL"
equals "首次預熱記下 origin/main sha" "$(cat "$STATE")" "$(origin_sha)"
equals "首次預熱後沒有重試記錄" "$(exists "$RETRY")" "no"
equals "專用 worktree 停在 origin/main" "$(git -C "$WT" rev-parse HEAD)" "$(origin_sha)"
equals "專用 worktree 有 marker" "$(exists "$(marker_of)")" "yes"
equals "專用 worktree 的 web/dist 是 stub（gitignore，不弄髒工作樹）" "$(exists "$WT/web/dist/index.html")" "yes"
equals "首次預熱沒有跑到本機 cargo" "$(count LOCAL-CARGO)" "0"
contains "首次預熱有說明" "$out" "已在 外部編譯主機 預熱完成"

# 2. 同一個 sha 再來一輪 → 不再叫 helper。
out=$(run); rc=$?
equals "同 sha 再跑 exit 0" "$rc" "0"
equals "同 sha 再跑不叫 helper" "$(count daemon)" "3"
contains "同 sha 有說明" "$out" "已經預熱過"

# 3. main 有新 commit → 再預熱一次（沿用同一棵 marker worktree），狀態跟著前進。
push_new_main two
out=$(run)
equals "main 更新後再預熱，helper 總共 6 次" "$(count daemon)" "6"
equals "main 更新後狀態記新 sha" "$(cat "$STATE")" "$(origin_sha)"
equals "main 更新後 worktree 跟到新 sha" "$(git -C "$WT" rev-parse HEAD)" "$(origin_sha)"

# 4. helper 125（r7 沒啟用／連不上）→ 跳過、不記狀態、不在本機跑。
reset_state
out=$(FAKE_RC=125 run); rc=$?
equals "125 exit 0（不算失敗）" "$rc" "0"
equals "125 第一步之後不再叫" "$(count daemon)" "1"
equals "125 不記狀態，下一輪再試" "$(exists "$STATE")" "no"
equals "125 沒有重試記錄（不算失敗次數）" "$(exists "$RETRY")" "no"
equals "125 絕不退回本機 cargo" "$(count LOCAL-CARGO)" "0"
contains "125 有說明" "$out" "跳過，不在本機跑"

# 5. helper 75（r7 名額滿）→ 跳過、不記狀態。
reset_state
out=$(FAKE_RC=75 run); rc=$?
equals "75 exit 0" "$rc" "0"
equals "75 不記狀態" "$(exists "$STATE")" "no"
equals "75 絕不退回本機 cargo" "$(count LOCAL-CARGO)" "0"

# 6. helper 126（設定錯）→ 跳過、不記狀態。
reset_state
out=$(FAKE_RC=126 run); rc=$?
equals "126 exit 0" "$rc" "0"
equals "126 不記狀態" "$(exists "$STATE")" "no"
contains "126 說明要修設定" "$out" "請修 [build.remote]"

# 7. 124（逾時）是暫時失敗：每輪一次、最多 3 次，第 2、3 次之間隔一輪，用完放棄同一個 sha。
reset_state
out=$(FAKE_RC=124 run); rc1=$?
equals "逾時第 1 輪 exit 0" "$rc1" "0"
equals "逾時第 1 輪 check 失敗就不跑後面的步" "$(count daemon)" "1"
equals "逾時第 1 輪後重試記錄 sha 1 0" "$(cat "$RETRY")" "$(origin_sha) 1 0"
equals "逾時不記成功" "$(exists "$STATE")" "no"
FAKE_RC=124 run >/dev/null
equals "第 2 輪逾時，記錄 sha 2 1" "$(cat "$RETRY")" "$(origin_sha) 2 1"
out=$(FAKE_RC=124 run)
equals "第 3 輪退避，記錄 sha 2 0、不叫 helper" "$(cat "$RETRY")" "$(origin_sha) 2 0"
equals "退避輪沒叫 helper" "$(count daemon)" "2"
contains "退避有說明" "$out" "退避中"
FAKE_RC=124 run >/dev/null
equals "第 4 輪再試，記錄 sha 3 2" "$(cat "$RETRY")" "$(origin_sha) 3 2"
out=$(FAKE_RC=124 run)
contains "用完 3 次就放棄" "$out" "放棄"
equals "放棄後總共只叫 3 次 helper" "$(count daemon)" "3"

# 8. 逾時之後下一輪成功：記成功 sha、清掉重試記錄。
reset_state
printf '124\n0\n0\n0\n' > "$RC_FILE"
run >/dev/null
equals "逾時後成功：第 1 輪有重試記錄" "$(cat "$RETRY")" "$(origin_sha) 1 0"
out=$(run); rc=$?
equals "逾時後下一輪成功 exit 0" "$rc" "0"
equals "逾時後下一輪記成功 sha" "$(cat "$STATE")" "$(origin_sha)"
equals "成功後清掉重試記錄" "$(exists "$RETRY")" "no"

# 9. 被訊號中斷（137）也是暫時失敗，記重試，不記成功。
reset_state
FAKE_RC=137 run >/dev/null
equals "被中斷記重試 sha 1 0" "$(cat "$RETRY")" "$(origin_sha) 1 0"
equals "被中斷不記成功" "$(exists "$STATE")" "no"

# 10. 其他 cargo 失敗（101，編譯錯）→ 這版有問題，直接放棄，不重試。
reset_state
FAKE_RC=101 run >/dev/null
equals "編譯失敗直接記放棄" "$(cat "$RETRY")" "$(origin_sha) 3 0"
out=$(FAKE_RC=0 run)
equals "編譯失敗後同 sha 不再叫 helper" "$(count daemon)" "1"
contains "編譯失敗後有放棄說明" "$out" "放棄"

# 11. 同名的既有 worktree 不是預熱腳本建的（沒有 marker）→ 拒絕、記 log、不動它（WIP 留著）。
reset_state
git -C "$ROOT/repo" worktree add -q --detach "$WT" "$(origin_sha)"
echo wip > "$WT/wip.txt"
out=$(run); rc=$?
equals "無 marker 的 worktree exit 0" "$rc" "0"
equals "無 marker 的 worktree 不叫 helper" "$(count daemon)" "0"
equals "無 marker 的 worktree 的 WIP 還在" "$(cat "$WT/wip.txt")" "wip"
contains "無 marker 有拒絕說明" "$out" "拒絕，不動它"

# 12. 有 marker 但有未提交改動 → 拒絕、WIP 留著。
reset_state
run >/dev/null
rm -f "$STATE"
echo wip > "$WT/wip.txt"
out=$(run); rc=$?
equals "髒的預熱 worktree exit 0" "$rc" "0"
equals "髒的預熱 worktree 的 WIP 還在" "$(cat "$WT/wip.txt")" "wip"
contains "髒的預熱 worktree 有說明" "$out" "未提交的改動"

# 13. 有 marker 但掛在分支上（不是 detached）→ 拒絕。
reset_state
run >/dev/null
rm -f "$STATE"
git -C "$WT" checkout -q -b someone-else
out=$(run)
contains "分支上的預熱 worktree 拒絕" "$out" "不是 detached HEAD"
equals "分支上的預熱 worktree 不叫 helper" "$(count daemon)" "3"

# 14. 別的 repo 的 worktree（common dir 不同）→ 拒絕。
reset_state
git init -q -b main "$ROOT/other"
git -C "$ROOT/other" commit -q --allow-empty -m other
git -C "$ROOT/other" worktree add -q --detach "$WT" HEAD
touch "$(git -C "$WT" rev-parse --path-format=absolute --git-dir)/${MARKER_NAME}"
out=$(run); rc=$?
equals "別的 repo 的 worktree exit 0" "$rc" "0"
contains "別的 repo 的 worktree 拒絕" "$out" "不同 repo 的 worktree"
equals "別的 repo 的 worktree 不叫 helper" "$(count daemon)" "0"
git -C "$ROOT/other" worktree remove --force "$WT" 2>/dev/null
rm -rf "$WT" "$ROOT/other"

# 15. daemon 不在 → 跳過、不記狀態、不在本機跑。
reset_state
out=$(DAEMON_OVERRIDE="$ROOT/no-such-daemon" run); rc=$?
equals "沒有 daemon exit 0" "$rc" "0"
equals "沒有 daemon 不記狀態" "$(exists "$STATE")" "no"
contains "沒有 daemon 有說明" "$out" "找不到 daemon"

# 16. 上一輪還握著鎖 → 這一輪直接退出，什麼都不做。
reset_state
mkdir -p "$(dirname "$STATE")"
flock -x "${STATE}.lock" -c 'sleep 3' &
holder=$!
sleep 0.5
out=$(run); rc=$?
wait "$holder"
equals "上一輪還在跑 exit 0" "$rc" "0"
equals "上一輪還在跑不叫 helper" "$(count daemon)" "0"
contains "上一輪還在跑有說明" "$out" "上一輪還在跑"

# 17. fetch 失敗（origin 不見了）→ 跳過、不碰狀態。
reset_state
git -C "$ROOT/repo" remote set-url origin "$ROOT/none.git"
out=$(run); rc=$?
git -C "$ROOT/repo" remote set-url origin "$ROOT/origin.git"
equals "fetch 失敗 exit 0" "$rc" "0"
equals "fetch 失敗不叫 helper" "$(count daemon)" "0"
contains "fetch 失敗有說明" "$out" "fetch origin main 失敗"

# 18. 多台（#1956）：逐台各帶 --host 跑三步，狀態各記一份；一台 125 不擋另一台，下一輪只重跑沒成功的那台。
reset_state
printf '0\n0\n0\n125\n' > "$RC_FILE"
out=$(FAKE_HOSTS="local r7@r7:22" run); rc=$?
equals "多台 exit 0" "$rc" "0"
equals "多台第一台帶 --host" "$(sed -n 1p "$LOG")" "daemon remote-cargo --config $CFG --cwd $WT --host local -- check -p agents-managerd --all-targets --locked"
equals "多台第二台帶 --host" "$(sed -n 4p "$LOG")" "daemon remote-cargo --config $CFG --cwd $WT --host r7@r7:22 -- check -p agents-managerd --all-targets --locked"
equals "多台：成功的那台記 sha" "$(cat "${STATE}.local")" "$(origin_sha)"
equals "多台：125 的那台不記" "$(exists "${STATE}.r7_r7_22")" "no"
contains "多台：說得出是哪一台" "$out" "r7@r7:22 不可用"
out=$(FAKE_HOSTS="local r7@r7:22" run)
equals "下一輪只重跑沒成功的那台" "$(grep -c -- '--host r7@r7:22' "$LOG")" "4"
equals "成功過的那台不再跑" "$(grep -c -- '--host local' "$LOG")" "3"
equals "第二台這次成功記 sha" "$(cat "${STATE}.r7_r7_22")" "$(origin_sha)"
contains "成功過的那台有說明" "$out" "已經預熱過（local）"

# 19. 單元檔與對照表：timer 每 600 秒、service 叫的是安裝後的 bin 名稱，對照表的來源檔都存在。
equals "timer 每 600 秒" "$(grep -c '^OnUnitActiveSec=600s$' "$UNIT_DIR/com.agm.remote-cargo-prewarm.timer")" "1"
equals "service 叫安裝後的腳本" "$(grep -c '^ExecStart=.*/bin/remote-cargo-prewarm.sh$' "$UNIT_DIR/com.agm.remote-cargo-prewarm.service")" "1"
MANIFEST="$HERE/install-manifest.tsv"
equals "對照表有腳本列" "$(grep -c '^scripts/ops/remote-cargo-prewarm.sh  *bin/remote-cargo-prewarm.sh  *linux$' "$MANIFEST")" "1"
equals "對照表有 timer 與 service 列" "$(grep -c '^scripts/ops/systemd/com.agm.remote-cargo-prewarm\.\(service\|timer\) ' "$MANIFEST")" "2"

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
