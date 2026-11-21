#!/bin/bash
set -u

REPO="${AGM_PREWARM_REPO:-$HOME/project/agents-manager}"
WT="${AGM_PREWARM_WORKTREE:-$REPO/.claude/worktrees/remote-cargo-prewarm}"
CFG="${AGM_PREWARM_CONFIG:-$HOME/.config/agents-manager/config.toml}"
DAEMON="${AGM_PREWARM_DAEMON:-$REPO/target/release/agents-managerd}"
STATE="${AGM_PREWARM_STATE:-$HOME/.cache/agents-manager/remote-cargo-prewarm.sha}"
FETCH_TIMEOUT="${AGM_PREWARM_FETCH_TIMEOUT:-300}"
PS_CMD="${AGM_PREWARM_PS:-ps -eo args=}"
MAX_ATTEMPTS=3
MARKER="agm-remote-cargo-prewarm"

log() {
    echo "remote-cargo-prewarm: $*"
}

others_building() {
    $PS_CMD 2>/dev/null | awk -v wt="$WT" '
        index($0, " remote-cargo --config ") && index($0, " --cwd ") {
            n = split($0, a, " ")
            for (i = 1; i < n; i++) if (a[i] == "--cwd") { if (a[i + 1] != wt) c++; break }
        }
        END { print c + 0 }'
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

pending=()
for h in "${hosts[@]}"; do
    st="$(state_of "$h")"
    rt="${st}.retry"
    where="$(where_of "$h")"
    if [ "$sha" = "$(cat "$st" 2>/dev/null || true)" ]; then
        log "origin/main ${sha} 已經預熱過（${where}）"
        continue
    fi
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

n="$(others_building)"
if [ "$n" -gt 0 ]; then
    log "有 ${n} 個 bot 正在用編譯名額，這次不預熱（不記失敗次數，下一輪再試）"
    exit 0
fi

prepare_worktree || exit 0

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
        n="$(others_building)"
        if [ "$n" -gt 0 ]; then
            log "有 ${n} 個 bot 開始用編譯名額，預熱讓開（${where} 停在 ${step} 之前，不記失敗次數）"
            rc=75
            break
        fi
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
