#!/bin/bash
# 預熱外部編譯主機的 cargo 快取（issue #1855；多台時逐台，#1956）：main 的 HEAD 變了，就在每台先跑一次 check 與 test --no-run，
# 讓之後 child 開工時吃到的是熱的快取，不是冷啟動（實測 r7 冷的整樹 check 3 分 10 秒，熱的一行改動 4.8 秒）。
#
# 由 systemd user timer `com.agm.remote-cargo-prewarm.timer` 定期叫一次。每次：
#   1. 拿不到鎖＝上一輪還在跑，直接退出（同時最多 1 輪）。
#   2. fetch origin main；跟狀態檔記的 sha 一樣就退出（成功的 sha 只預熱一次）。
#   3. 專用 worktree（`AGM_PREWARM_WORKTREE`）必須是這支腳本自己建的：gitdir 裡有 marker、同一個 git common dir、
#      detached、乾淨。不符合就拒絕、記 log、**不碰它**（同名的既有 worktree 可能有人的 WIP）。
#   4. 逐步呼叫 daemon 的 `remote-cargo` helper，目標集合跟 scripts/check.sh 的 daemon／xtask／crates 測試一樣
#      （不含 desktop：它的 build script 要打包流程才會放進來的 sidecar）；每一步都是 `-p <包>`，跟 check.sh 同樣的 feature 統一。
#   5. **只在外部編譯主機跑，絕不退回 shim 的本機名額**：helper 回 125（沒啟用／連不上）、75（名額滿）、126（設定錯）只跳過、不記次數、下一輪再試。
#      多台（`remote-cargo --list-hosts` 列出兩台以上，#1956）時逐台各帶 `--host <名字>` 跑，一台失敗不擋下一台。
#   6. 成功＝記成功的 sha（`STATE`；多台時每台一份 `STATE.<名字>`）。暫時失敗（124 逾時，或 128 以上的訊號中斷）記在 `<狀態檔>.retry`：同一個 sha 最多 3 次，
#      第 2、3 次之間各隔一輪（退避）；用完就放棄這個 sha，等下一版 main。其他 cargo 失敗（例如編譯錯）視為這個 sha 有問題，直接放棄，不重試。
#
# 快取 key：遠端目錄 hash 的種子是 git common dir（remote_cargo.rs `cache_hash`，#1856），同一個 repo 的預熱 worktree
# 與 child worktree 對到同一份 `shared/`，所以預熱熱的快取 child 用得到。見 docs/SPEC.md 外部 cargo 那節。
#
# 可覆寫（測試用）：AGM_PREWARM_REPO、AGM_PREWARM_WORKTREE、AGM_PREWARM_CONFIG、AGM_PREWARM_DAEMON、AGM_PREWARM_STATE、
# AGM_PREWARM_FETCH_TIMEOUT。
set -u

REPO="${AGM_PREWARM_REPO:-$HOME/project/agents-manager}"
WT="${AGM_PREWARM_WORKTREE:-$REPO/.claude/worktrees/remote-cargo-prewarm}"
CFG="${AGM_PREWARM_CONFIG:-$HOME/.config/agents-manager/config.toml}"
DAEMON="${AGM_PREWARM_DAEMON:-$REPO/target/release/agents-managerd}"
STATE="${AGM_PREWARM_STATE:-$HOME/.cache/agents-manager/remote-cargo-prewarm.sha}"
FETCH_TIMEOUT="${AGM_PREWARM_FETCH_TIMEOUT:-300}"
MAX_ATTEMPTS=3
MARKER="agm-remote-cargo-prewarm"

log() {
    echo "remote-cargo-prewarm: $*"
}

mkdir -p "$(dirname "$STATE")" || exit 0
exec 9>"${STATE}.lock"
if ! flock -n 9; then
    log "上一輪還在跑，這次不重複"
    exit 0
fi

if ! timeout "$FETCH_TIMEOUT" git -C "$REPO" fetch --quiet origin main; then
    log "fetch origin main 失敗，這次跳過"
    exit 0
fi
sha="$(git -C "$REPO" rev-parse --verify --quiet origin/main)" || {
    log "讀不到 origin/main，這次跳過"
    exit 0
}

if [ ! -x "$DAEMON" ]; then
    log "找不到 daemon ${DAEMON}，這次跳過（不改在本機跑）"
    exit 0
fi

# 逐台預熱（#1956）：helper 照設定順序列出每台建置主機；只有一台（或舊版 helper 不認得 --list-hosts）時照舊不帶 --host，
# 狀態檔也照舊；多台時每台各帶 `--host <名字>`，狀態與重試記錄各自一份（`<STATE>.<名字>`）。
hosts=()
while IFS= read -r h; do
    [ -n "$h" ] && hosts+=("$h")
done < <("$DAEMON" remote-cargo --config "$CFG" --list-hosts 2>/dev/null)
[ "${#hosts[@]}" -ge 2 ] || hosts=("")

state_of() {
    if [ -z "$1" ]; then printf '%s' "$STATE"; else printf '%s.%s' "$STATE" "$(printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '_')"; fi
}
where_of() {
    if [ -z "$1" ]; then printf '外部編譯主機'; else printf '%s' "$1"; fi
}

# 每台先看要不要預熱（成功過、放棄了、退避中就跳過）；要的才放進 pending。
pending=()
for h in "${hosts[@]}"; do
    st="$(state_of "$h")"
    rt="${st}.retry"
    where="$(where_of "$h")"
    if [ "$sha" = "$(cat "$st" 2>/dev/null || true)" ]; then
        log "origin/main ${sha} 已經預熱過（${where}）"
        continue
    fi
    # 暫時失敗的重試記錄：「sha 次數 還要跳過的輪數」。sha 換了就從頭算。
    r_count=0
    r_skip=0
    if read -r r_sha r_n r_s 2>/dev/null < "$rt" && [ "$r_sha" = "$sha" ]; then
        r_count="$r_n"
        r_skip="$r_s"
    fi
    if [ "$r_count" -ge "$MAX_ATTEMPTS" ]; then
        log "origin/main ${sha} 在 ${where} 暫時失敗已 ${r_count} 次，放棄；等下一版 main"
        continue
    fi
    if [ "$r_skip" -gt 0 ]; then
        printf '%s %s %s\n' "$sha" "$r_count" "$((r_skip - 1))" > "${rt}.tmp" && mv "${rt}.tmp" "$rt"
        log "origin/main ${sha} 在 ${where} 退避中，還要跳過 ${r_skip} 輪"
        continue
    fi
    pending+=("$h")
done
[ "${#pending[@]}" -gt 0 ] || exit 0

# 專用 worktree 的身份檢查：只接受這支腳本建的那棵（marker 放在 gitdir，不進工作樹，所以不會弄髒它）。
prepare_worktree() {
    local repo_common wt_common gitdir
    repo_common="$(git -C "$REPO" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" || return 1
    if [ ! -e "$WT" ]; then
        git -C "$REPO" worktree add --quiet --detach "$WT" "$sha" || return 1
        gitdir="$(git -C "$WT" rev-parse --path-format=absolute --git-dir)" || return 1
        touch "${gitdir}/${MARKER}"
        return 0
    fi
    refuse() {
        log "${WT} 不是預熱專用的 worktree（${1}），拒絕，不動它"
        return 1
    }
    [ -f "$WT/.git" ] || { refuse "不是 git worktree"; return 1; }
    wt_common="$(git -C "$WT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" || { refuse "讀不到 git 目錄"; return 1; }
    [ "$wt_common" = "$repo_common" ] || { refuse "不同 repo 的 worktree"; return 1; }
    gitdir="$(git -C "$WT" rev-parse --path-format=absolute --git-dir 2>/dev/null)" || { refuse "讀不到 gitdir"; return 1; }
    [ -f "${gitdir}/${MARKER}" ] || { refuse "缺預熱腳本的 marker"; return 1; }
    if git -C "$WT" symbolic-ref -q HEAD >/dev/null; then refuse "不是 detached HEAD"; return 1; fi
    [ -z "$(git -C "$WT" status --porcelain)" ] || { refuse "有未提交的改動"; return 1; }
    git -C "$WT" checkout --quiet --detach "$sha" || {
        log "checkout ${sha} 到 ${WT} 失敗，這次跳過"
        return 1
    }
}

prepare_worktree || exit 0

# check.sh 的 Rust 路徑也用臨時 stub：daemon 用 rust-embed 把 web/dist 編進去，預熱不跑 web build（web/dist 是 gitignore 的，不弄髒 worktree）。
if [ ! -f "$WT/web/dist/index.html" ]; then
    mkdir -p "$WT/web/dist" && printf '<!doctype html>\n' > "$WT/web/dist/index.html"
fi

warm() {
    if [ -z "$host" ]; then
        "$DAEMON" remote-cargo --config "$CFG" --cwd "$WT" -- "$@"
    else
        "$DAEMON" remote-cargo --config "$CFG" --cwd "$WT" --host "$host" -- "$@"
    fi
}

# 同 scripts/check.sh：daemon 的 check --all-targets、daemon／xtask 與每個 crates/* 的 test --no-run（逐包 -p，跟 check.sh 的 feature 統一一致）。
steps=(
    "check -p agents-managerd --all-targets --locked"
    "test -p agents-managerd --no-run --locked"
    "test -p xtask --no-run --locked"
)
for d in "$WT"/crates/*/; do
    [ -d "$d" ] || continue
    steps+=("test -p $(basename "$d") --no-run --locked")
done

record_success() {
    printf '%s\n' "$sha" > "${st}.tmp" && mv "${st}.tmp" "$st"
    rm -f "$rt"
}

record_transient() {
    local count=$((r_count + 1)) skip=$((r_count))
    printf '%s %s %s\n' "$sha" "$count" "$skip" > "${rt}.tmp" && mv "${rt}.tmp" "$rt"
    if [ "$count" -ge "$MAX_ATTEMPTS" ]; then
        log "預熱 ${sha} 在 ${where} 暫時失敗已 ${count} 次（exit ${rc}），放棄；等下一版 main"
    else
        log "預熱 ${sha} 在 ${where} 暫時失敗（exit ${rc}），第 ${count} 次，過 ${skip} 輪再試"
    fi
}

# 一台一台來（同一台的 shared/ 才會熱；各台互不影響，一台失敗不擋下一台）。
for host in "${pending[@]}"; do
    st="$(state_of "$host")"
    rt="${st}.retry"
    where="$(where_of "$host")"
    r_count=0
    if read -r r_sha r_n _ 2>/dev/null < "$rt" && [ "$r_sha" = "$sha" ]; then
        r_count="$r_n"
    fi
    rc=0
    for step in "${steps[@]}"; do
        # shellcheck disable=SC2086
        warm $step || rc=$?
        [ "$rc" = 0 ] || break
    done
    case "$rc" in
        0)
            log "origin/main ${sha} 已在 ${where} 預熱完成（${#steps[@]} 步）"
            record_success
            ;;
        125)
            log "${where} 不可用：沒啟用、連不上（helper 125）：跳過，不在本機跑，下一輪再試"
            ;;
        75)
            log "${where} 名額滿（helper 75）：跳過，下一輪再試"
            ;;
        126)
            log "外部編譯設定錯（helper 126）：跳過，下一輪再試，請修 [build.remote]"
            ;;
        124)
            record_transient
            ;;
        *)
            if [ "$rc" -ge 128 ]; then
                record_transient
            else
                log "預熱 ${sha} 在 ${where} 編譯失敗（exit ${rc}）：這版有問題，放棄，不重試"
                printf '%s %s %s\n' "$sha" "$MAX_ATTEMPTS" 0 > "${rt}.tmp" && mv "${rt}.tmp" "$rt"
            fi
            ;;
    esac
done
exit 0
