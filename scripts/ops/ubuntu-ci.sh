#!/usr/bin/env bash
# ubuntu 背景完整 CI（issue #716）：取代「每個 push 都等 GitHub Actions」。
#
# 由 systemd user timer 每分鐘叫一次（scripts/ops/ci/ubuntu-ci.{service,timer}）。每次：
#   1. 拿不到鎖＝上一輪還在跑，直接退出（同時最多 1 個 running）；握超過 AGM_CI_HUNG_AFTER 秒就 ops-alert ci_hung。
#   2. fetch origin/main；跟上次驗過的 sha 一樣就退出。
#   3. 只驗「現在最新的」main HEAD（latest-HEAD coalescing：中間被跳過的 sha 不補跑）。
#   4. 在常駐 clone 裡 checkout 該 sha、保留 target／node_modules 當暖快取，跑 scripts/check.sh（ob、ops、web、daemon）。
#      只跑「受影響的段」：每段各有一個已成功基準 ${CI_ROOT}/part-success/<段>，拿該基準到 HEAD 的累積差異問
#      scripts/ci-changed-parts.sh；差異跟這段無關就跳過（status.json 標 reused＋基準）。任何基準缺失／讀錯／非祖先、
#      分類失敗、未知路徑一律四段全跑；每 AGM_CI_FULL_INTERVAL 秒（預設 24h）無論如何補一次完整四段（同一個 sha 也會）。
#   5. 結果寫成 GitHub commit status（context `ubuntu-ci`）＋ ${CI_ROOT}/status.json＋每個 sha 一份 log。
#
# 剩餘空間低於 AGM_CI_MIN_FREE_GB（預設 20）時不跑，commit status 寫 error「磁碟不足，未執行」。
#
# 收尾的 agent 不等這一輪；要看結果就 `gh api repos/<repo>/commits/<sha>/status` 或讀 status.json。
set -euo pipefail

CI_ROOT="${AGM_CI_ROOT:-${HOME}/.cache/agents-manager/ci}"
REPO_URL="${AGM_CI_REPO_URL:-git@github.com:Eden-Sun/agents-manager.git}"
GH_REPO="${AGM_CI_GH_REPO:-Eden-Sun/agents-manager}"
CONTEXT="ubuntu-ci"
TIMEOUT="${AGM_CI_TIMEOUT:-45m}"
# timeout 只送 TERM；step 忽略 TERM 就永遠不結束、鎖永遠不放，之後每分鐘都「上一輪還在跑」。補 KILL。
KILL_AFTER="${AGM_CI_KILL_AFTER:-60s}"
KEEP_LOGS="${AGM_CI_KEEP_LOGS:-30}"
# 剩餘空間（GB）低於這個就不跑：同機的 worktree 各有 1–6 GB 的 target/，滿了 ops／daemon 會因 ENOSPC 假紅（c3914e46），
# 而紅燈會讓 last-sha 前進、同一個 sha 不再重試。
MIN_FREE_GB="${AGM_CI_MIN_FREE_GB:-20}"
# 已安裝的 ops 腳本（AGM bin）與 origin/main 的漂移檢查間隔（秒；0＝不檢查）。見 ops_sync_check。
OPS_SYNC_INTERVAL="${AGM_CI_OPS_SYNC_INTERVAL:-21600}"
# 定期完整四段的間隔（秒；0＝不補）。以最近一次完整四段「跑完」（成功或失敗）的時間算，失敗後不會每分鐘重跑。
# 0＝明確關閉；非數字（含負值、空字串以外的亂字）不能悄悄把定期完整關掉，退回預設 86400 並警告。
FULL_INTERVAL="${AGM_CI_FULL_INTERVAL:-86400}"
case "${FULL_INTERVAL}" in
    ''|*[!0-9]*)
        echo "ubuntu-ci: AGM_CI_FULL_INTERVAL='${FULL_INTERVAL}' 不是非負整數，改用預設 86400" >&2
        FULL_INTERVAL=86400 ;;
esac
# daemon 要把 web/dist 嵌進去（rust-embed）。web 段沒跑（只改 Rust）時 checkout 後 dist 不存在（git clean 掉了），
# 在跑 daemon 前用本輪 HEAD 重建一次真的 bundle（只 build，不跑 tsc／oxlint／bun test）；不用舊 bundle、不用 stub。
WEB_BUNDLE_CMD="${AGM_CI_WEB_BUNDLE_CMD:-cd web && bun install --frozen-lockfile && bunx vite build}"
PARTS="ob ops web daemon"

# 網路半開時 clone／fetch 會卡到天荒地老、整個握著鎖，之後每分鐘都「上一輪還在跑」而 CI／自動部署靜默停擺（#946）：
# 兩者都要有時限（timeout 是 GNU 版，會把訊號送給整個行程群組，git 的子行程一起收掉）。
# AGM_CI_GIT_BIN 是測試接縫（假 git 的 fetch 睡很久）。
GIT_BIN="${AGM_CI_GIT_BIN:-git}"
FETCH_TIMEOUT="${AGM_CI_FETCH_TIMEOUT:-300}"
CLONE_TIMEOUT="${AGM_CI_CLONE_TIMEOUT:-600}"
# 鎖被占著超過這麼久（秒）就當它卡住了：預設 = unit 的 TimeoutStartSec（4 段 × 45m + 15m）。
HUNG_AFTER="${AGM_CI_HUNG_AFTER:-11700}"

# 拿不到鎖時：上一輪是不是卡住了？卡住（running-since 超過門檻）就喊人。ops-alert 同 source+reason 每小時最多一則，
# 每分鐘照喊也不會灌滿 inbox。喊人失敗、clone 還沒有、running-since 讀不懂一律吞掉，不影響退出碼。
alert_if_hung() {
    local since now
    since="$(cat "${CI_ROOT}/running-since" 2>/dev/null || true)"
    case "${since}" in *[!0-9]*|'') return 0 ;; esac
    now="$(date +%s)"
    [ $((now - since)) -ge "${HUNG_AFTER}" ] || return 0
    [ -f "${CI_ROOT}/repo/scripts/agm.py" ] || return 0
    timeout -k 5 60 python3 -B "${CI_ROOT}/repo/scripts/agm.py" ops-alert --source ubuntu-ci --reason ci_hung \
        --detail "ubuntu-ci 上一輪從 $(date -u -d "@${since}" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo "${since}") 起還握著鎖（超過 ${HUNG_AFTER} 秒）；CI 與自動部署停擺中。查 systemctl --user status ubuntu-ci.service，必要時 systemctl --user stop ubuntu-ci.service" \
        >/dev/null 2>&1 || true
}

mkdir -p "${CI_ROOT}/logs"
exec 9>"${CI_ROOT}/lock"
if ! flock -n 9; then
    alert_if_hung || true
    exit 0
fi
# 這一輪從什麼時候開始握著鎖（上面 alert_if_hung 讀它）。
date +%s > "${CI_ROOT}/running-since" 2>/dev/null || true

if [ ! -d "${CI_ROOT}/repo/.git" ]; then
    timeout -k 10 "${CLONE_TIMEOUT}" "${GIT_BIN}" clone -q "${REPO_URL}" "${CI_ROOT}/repo"
fi
cd "${CI_ROOT}/repo"
timeout -k 10 "${FETCH_TIMEOUT}" "${GIT_BIN}" fetch -q origin main

# 送一則 commit status（sha 由參數給，補送舊 sha 也用它）。暫時連不上就重試幾次（間隔 AGM_CI_STATUS_RETRY_SLEEP 秒）；
# 還是不行回非零，由呼叫端決定要不要記下來補送。
post_status() {
    local at="$1" state="$2" desc="$3" i
    for i in 1 2 3; do
        if gh api -X POST "repos/${GH_REPO}/statuses/${at}" -f state="${state}" -f context="${CONTEXT}" \
            -f description="${desc}" >/dev/null 2>&1; then
            return 0
        fi
        [ "${i}" -ge 3 ] || sleep "${AGM_CI_STATUS_RETRY_SLEEP:-3}"
    done
    echo "commit status 寫不上去（${state}）" >&2
    return 1
}

retain_unposted_line() {
    if ! printf '%s\n' "$2" >> "$1"; then
        echo "ubuntu-ci: 補送佇列暫存寫失敗，保留原 unposted" >&2
        return 1
    fi
}
defer_unposted_line() {
    if ! retain_unposted_line "$1" "$2"; then
        rm -f -- "$1"
        return 1
    fi
}

# 上一輪沒送上去的結果（見 final_status）先逐筆補送：要在「同一個 sha 就退出」之前，不然 main 靜止時永遠補不到。
post_unposted() {
    local line usha rest ustate udesc retry_file invalid=0
    [ -s "${CI_ROOT}/unposted" ] || return 0
    retry_file="$(mktemp "${CI_ROOT}/unposted.XXXXXX")" || return 1
    while IFS= read -r line || [ -n "${line:-}" ]; do
        [ -n "${line}" ] || continue
        case "${line}" in
            *$'\t'*) usha="${line%%$'\t'*}"; rest="${line#*$'\t'}" ;;
            *) invalid=1; defer_unposted_line "${retry_file}" "${line}" || return 1; continue ;;
        esac
        case "${rest}" in
            *$'\t'*) ustate="${rest%%$'\t'*}"; udesc="${rest#*$'\t'}" ;;
            *) invalid=1; defer_unposted_line "${retry_file}" "${line}" || return 1; continue ;;
        esac
        case "${usha}" in *[!0-9a-f]*|'') invalid=1; defer_unposted_line "${retry_file}" "${line}" || return 1; continue ;; esac
        [ "${#usha}" = 40 ] || { invalid=1; defer_unposted_line "${retry_file}" "${line}" || return 1; continue; }
        case "${ustate}" in pending|success|failure|error) ;; *) invalid=1; defer_unposted_line "${retry_file}" "${line}" || return 1; continue ;; esac
        if ! post_status "${usha}" "${ustate}" "${udesc}"; then
            defer_unposted_line "${retry_file}" "${line}" || return 1
        fi
    done < "${CI_ROOT}/unposted"
    [ "${invalid}" = 0 ] || echo "ubuntu-ci: unposted 有格式錯誤記錄，已保留供檢查（${CI_ROOT}/unposted）" >&2
    if [ -s "${retry_file}" ]; then
        if ! mv -f -- "${retry_file}" "${CI_ROOT}/unposted"; then
            echo "ubuntu-ci: 無法更新補送佇列，保留原 unposted" >&2
            return 1
        fi
    else
        if ! rm -f -- "${retry_file}" "${CI_ROOT}/unposted"; then
            echo "ubuntu-ci: 無法清除已補送的佇列" >&2
            return 1
        fi
    fi
}
post_unposted || true

# 已安裝的 ops 腳本跟 origin/main 的漂移檢查（#418 的 `agm ops-sync --check --alert`）：文件寫「巡檢每天跑一次」，但沒有任何東西在排程它，
# outbox-gc.sh 停在舊版、repo 的修正一直沒生效也沒人知道。這支每分鐘都會跑、又是從 CI clone 直接執行（不用安裝），所以順手問一次。
# 只偵測回報：不安裝、不改 AGM bin；有落差就由 ops-sync 自己推 ops_alert（同一小時一則）。結果寫在 ${CI_ROOT}/ops-sync.json，
# 檢查本身壞掉也不能影響 CI 的 status／last-sha，所以失敗一律吞掉。不靠新 commit 觸發（沒人裝的話 main 靜止也要再提醒）。
ops_sync_check() {
    local now last rc=0
    [ "${OPS_SYNC_INTERVAL}" -gt 0 ] 2>/dev/null || return 0
    now="$(date +%s)"
    last="$(cat "${CI_ROOT}/ops-sync.last" 2>/dev/null || echo 0)"
    [ $((now - last)) -ge "${OPS_SYNC_INTERVAL}" ] || return 0
    echo "${now}" > "${CI_ROOT}/ops-sync.last"
    timeout -k 10 120 python3 -B "${CI_ROOT}/repo/scripts/agm.py" ops-sync --check --alert --repo "${CI_ROOT}/repo" \
        > "${CI_ROOT}/ops-sync.json" 2> "${CI_ROOT}/ops-sync.err" || rc=$?
    # rc 0＝一致、1＝有落差（已 alert）；其他是檢查自己沒跑成，看 ops-sync.err。
    [ "${rc}" -le 1 ] || echo "ubuntu-ci: ops-sync 檢查沒跑成（rc=${rc}），見 ${CI_ROOT}/ops-sync.err" >&2
    return 0
}
ops_sync_check || true

# 定期完整四段到期了嗎？紀錄（full-success-at／full-attempt-at 的 epoch 秒，舊版沒有就看 full-success-sha 的 mtime）都讀不到＝到期。
full_due() {
    local now t f latest=0
    [ "${FULL_INTERVAL}" -gt 0 ] || return 1
    now="$(date +%s)"
    # 未來的時間戳（時鐘跳回、檔案寫壞）一律當沒有紀錄：否則 now-latest 為負、定期完整永遠不到期。
    for f in full-success-at full-attempt-at; do
        t="$(cat "${CI_ROOT}/${f}" 2>/dev/null || echo 0)"
        case "${t}" in *[!0-9]*|'') t=0 ;; esac
        [ "${t}" -le "${now}" ] || t=0
        [ "${t}" -le "${latest}" ] || latest="${t}"
    done
    if [ "${latest}" = 0 ] && [ -f "${CI_ROOT}/full-success-sha" ]; then
        latest="$(stat -c %Y "${CI_ROOT}/full-success-sha" 2>/dev/null || echo 0)"
        case "${latest}" in *[!0-9]*|'') latest=0 ;; esac
        [ "${latest}" -le "${now}" ] || latest=0
    fi
    [ $((now - latest)) -ge "${FULL_INTERVAL}" ]
}

sha="$(git rev-parse origin/main)"
last="$(cat "${CI_ROOT}/last-sha" 2>/dev/null || true)"
force_full=0
if full_due; then force_full=1; fi
if [ "${sha}" = "${last}" ] && [ "${force_full}" = 0 ]; then
    exit 0
fi

# 回報失敗不能讓整輪中斷：GitHub 暫時連不上時，結果仍寫在 status.json。
status() {
    post_status "${sha}" "$1" "$2" || true
}

# 最後的結果（success／failure／error）一定要送到：送不上去就附加到 ${CI_ROOT}/unposted（每行 sha、state、description，tab 分隔），
# 下一輪（不管有沒有新 commit、也不重跑檢查）先補送。不然 last-sha 已經前進，這個 sha 在 GitHub 上永遠停在 pending，
# 而 status.json 卻寫著 success。
rewrite_unposted_sha() {
    local target="$1" action="$2" state="${3:-}" desc="${4:-}" line tmp
    if [ "${action}" = remove ] && [ ! -s "${CI_ROOT}/unposted" ]; then
        return 0
    fi
    tmp="$(mktemp "${CI_ROOT}/unposted.XXXXXX")" || return 1
    if [ -s "${CI_ROOT}/unposted" ]; then
        while IFS= read -r line || [ -n "${line:-}" ]; do
            case "${line}" in
                "${target}"$'\t'*) continue ;;
            esac
            if ! printf '%s\n' "${line}" >> "${tmp}"; then
                rm -f -- "${tmp}"
                return 1
            fi
        done < "${CI_ROOT}/unposted"
    fi
    if [ "${action}" = write ] && ! printf '%s\t%s\t%s\n' "${target}" "${state}" "${desc}" >> "${tmp}"; then
        rm -f -- "${tmp}"
        return 1
    fi
    if [ -s "${tmp}" ]; then
        if ! mv -f -- "${tmp}" "${CI_ROOT}/unposted"; then
            rm -f -- "${tmp}"
            return 1
        fi
    elif ! rm -f -- "${tmp}" "${CI_ROOT}/unposted"; then
        return 1
    fi
}

final_status() {
    if post_status "${sha}" "$1" "$2"; then
        # This completed run supersedes any older queued result for the same SHA. Leaving it behind
        # would let a later replay overwrite this newer GitHub status.
        if ! rewrite_unposted_sha "${sha}" remove; then
            rewrite_unposted_sha "${sha}" write "$1" "$2"
        fi
    else
        # Keep the newest final result per SHA, including when a prior result is still queued.
        rewrite_unposted_sha "${sha}" write "$1" "$2"
    fi
}


# 放進手寫 JSON 字串前的跳脫：反斜線、雙引號、控制字元（description 取自 step 標題，什麼字元都可能有）。
json_str() {
    local v="$1"
    v="${v//\\/\\\\}"
    v="${v//\"/\\\"}"
    printf '%s' "${v}" | LC_ALL=C tr -d '\000-\037'
}

# 取前 N 個字（不是位元組）：GNU `cut -c` 等於 `-b`，會把 UTF-8 的中文字切成半個。python3 壞掉就原樣輸出（不能讓彙總因此中斷）。
clip_chars() { # clip_chars <N> <字串>
    python3 -X utf8 -c 'import sys; sys.stdout.write(sys.argv[2][:int(sys.argv[1])])' "$1" "$2" 2>/dev/null || printf '%s' "$2"
}

# 保險（#763）：舊版測試與被 kill 的測試行程留下的 am-*／agm-* 暫存目錄／檔案（名字結尾是 26 碼 ULID）會慢慢塞滿根磁碟
# （2026-10-01 實測 44G、ubuntu-ci 因 ENOSPC 紅 19 條）。持有鎖時才清（ubuntu-ci 自己那一輪不會被誤刪），失敗一律吞掉。保守規則：
#   - 排在磁碟預檢**之前**：磁碟被這些東西塞滿的那一輪正是需要清的那一輪，排在後面就永遠清不到。
#   - 只認 ${TMPDIR}（沒設用 /tmp）底下第一層；${TMPDIR} 不是絕對路徑、不存在、或解析後是 / 或 ${HOME} 就整個不動（設錯不能照字面去刪）。
#   - 只處理真的目錄與一般檔案（find 預設不跟 symlink，-type d／f 也排除 symlink 本身），rm 不會跟進 symlink 刪到別處。
#   - 超過 6 小時沒動：目錄自己的 mtime 只在直接子項增減時才變，所以裡面還有 6 小時內動過的東西就跳過（有行程可能還在用）。
#   - 名字必須以 am- 或 agm- 開頭、並以有效的 26 碼 ULID（可再接一個 .副檔名）結尾；沒有 ULID 的 am-*（如 am-ops-test）、herdr-*、claude-* 都不碰。
# 僅 Linux（GNU find 的 -regextype；整支腳本本來就用 flock／timeout）：BSD find 不認得它時整段失敗被吞掉，等於不清、不會誤刪。
sweep_stale_tmp() {
    local root logical real home_real d
    root="${TMPDIR:-/tmp}"
    case "${root}" in /*) ;; *) return 0 ;; esac
    logical="$(cd "${root}" 2>/dev/null && pwd -L)" || return 0
    real="$(cd "${root}" 2>/dev/null && pwd -P)" || return 0
    # Reject a symlinked TMPDIR (including a symlink in a parent component); never sweep through it.
    [ "${logical}" = "${real}" ] || return 0
    home_real="$(cd "${HOME:-/nonexistent}" 2>/dev/null && pwd -P || true)"
    case "${real}" in /|"${home_real}") return 0 ;; esac
    if [ -n "${home_real}" ]; then
        case "${home_real}" in /) return 0 ;; esac
        case "${real}/" in "${home_real}/"*) return 0 ;; esac
        case "${home_real}/" in "${real}/"*) return 0 ;; esac
    fi
    while IFS= read -r -d '' d; do
        if [ -d "${d}" ] && [ -n "$(find "${d}" -mmin -360 -print -quit 2>/dev/null)" ]; then
            continue
        fi
        rm -rf -- "${d}" 2>/dev/null || true
    done < <(find "${real}" -mindepth 1 -maxdepth 1 \( -type d -o -type f \) -mmin +360 -regextype posix-extended \
        -regex '.*/(am|agm)-[A-Za-z0-9-]*[0-7][0-9A-HJKMNP-TV-Z]{25}(\.[a-z]+)?' -print0 2>/dev/null || true)
}
sweep_stale_tmp || true

# 磁碟預檢（在任何 checkout／clean／建置之前）：不足就寫 error、不前進 last-sha，空間恢復後同一個 sha 會重跑。
# 每分鐘都會再進來，所以同一個 sha 只送一次 status（disk-low-sha 記著）。
free_kb="$(df -Pk "${CI_ROOT}" | awk 'NR==2 {print $4}')"
free_gb=$(( ${free_kb:-0} / 1048576 ))
if [ "${free_gb}" -lt "${MIN_FREE_GB}" ]; then
    if [ "$(cat "${CI_ROOT}/disk-low-sha" 2>/dev/null || true)" != "${sha}" ]; then
        # 先記「這個 sha 已經報過」：磁碟全滿時後面的寫入（status.json）會失敗，記號寫不下去的話每分鐘都重送一次 error（#945）。
        echo "${sha}" > "${CI_ROOT}/disk-low-sha" 2>/dev/null || true
        status error "磁碟不足，未執行（剩 ${free_gb}G，門檻 ${MIN_FREE_GB}G）"
        printf '{"sha":"%s","state":"error","reason":"disk_low","free_gb":%s,"min_free_gb":%s,"at":"%s"}\n' \
            "${sha}" "${free_gb}" "${MIN_FREE_GB}" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "${CI_ROOT}/status.json" 2>/dev/null || true
    fi
    echo "ubuntu-ci: 磁碟不足（剩 ${free_gb}G，門檻 ${MIN_FREE_GB}G），不跑 ${sha}" >&2
    exit 0
fi
rm -f "${CI_ROOT}/disk-low-sha"

log="${CI_ROOT}/logs/${sha}.log"
started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
printf '{"sha":"%s","state":"running","started":"%s","log":"%s"}\n' "${sha}" "${started}" "$(json_str "${log}")" > "${CI_ROOT}/status.json"
status pending "ubuntu 完整 CI 執行中"

# pending 送出之後任何一步（checkout、clean、彙總）因 set -e 中斷，都不能讓 commit status 永遠停在 pending、
# status.json 永遠停在 running：離開時還沒寫完結果就補一個 error（last-sha 不動，下一輪會重試）。
finalized=0
on_exit() {
    local code=$?
    if [ "${finalized}" = 0 ]; then
        status error "ubuntu-ci 腳本中斷（rc=${code}），見 journal"
        printf '{"sha":"%s","state":"error","rc":%s,"started":"%s","log":"%s"}\n' \
            "${sha}" "${code}" "${started}" "$(json_str "${log}")" > "${CI_ROOT}/status.json"
    fi
}
trap on_exit EXIT

git checkout -q --detach -f "${sha}"
git clean -q -fdx -e target -e web/node_modules

is_sha() { case "$1" in *[!0-9a-f]*|'') return 1 ;; esac; [ "${#1}" = 40 ]; }
read_sha_file() { local v; v="$(cat "$1" 2>/dev/null || true)"; is_sha "${v}" || return 1; printf '%s' "${v}"; }
# 原子地寫基準：寫到一半被殺不能留下半個 sha（讀的人會當「讀錯」而全跑，但仍盡量不產生）。
write_sha_file() { # write_sha_file <路徑> <sha>
    local tmp
    mkdir -p "$(dirname "$1")" 2>/dev/null || return 1
    tmp="$(mktemp "$1.XXXXXX")" || return 1
    if printf '%s\n' "$2" > "${tmp}" && mv -f -- "${tmp}" "$1"; then return 0; fi
    rm -f -- "${tmp}"
    return 1
}

# Only a fully tested ancestor can justify skipping a documentation-only descendant. A failed
# last-sha or another skipped SHA is never promoted to this baseline. Any unreadable input falls
# back to the full suite, including renames out of a tested path and classifier failures.
docs_only=0
full_success="$(cat "${CI_ROOT}/full-success-sha" 2>/dev/null || true)"
docs_only_since_full_success() {
    local files parts
    is_sha "${full_success}" || return 1
    git merge-base --is-ancestor "${full_success}" "${sha}" || return 1
    files="$(git -c core.quotePath=false diff --no-renames --name-only "${full_success}" "${sha}")" || return 1
    [ -n "${files}" ] || return 1
    parts="$(printf '%s\n' "${files}" | bash scripts/ci-changed-parts.sh)" || return 1
    [ -z "${parts}" ]
}

# 受影響的段：每段拿「自己最後一次成功的 sha」（part-success/<段>，必須是 HEAD 的祖先）到 HEAD 的累積差異問共用分類器，
# 這樣中間失敗或沒驗過的提交不會漏掉（失敗的段基準不前進，下一個提交的累積差異還含著它）。
# mode：full（四段全跑）｜scoped（只跑受影響的段）｜reuse（沒有任何段受影響，但差異不只文件）｜docs_only。
# 完整成功基準 full-success-sha 一律要有效：scoped／reuse 的 description 要引用它（ci-pr 靠它回查完整祖先）。
declare -A part_state part_base
run_parts=""
mode=full
select_reason=""
select_full() { mode=full; select_reason="$1"; run_parts="${PARTS}"; }
select_parts() {
    local p cp files out tok
    run_parts=""
    if [ "${force_full}" = 1 ]; then select_full periodic; return 0; fi
    if docs_only_since_full_success; then mode=docs_only; select_reason=docs_only; return 0; fi
    is_sha "${full_success}" && git merge-base --is-ancestor "${full_success}" "${sha}" 2>/dev/null \
        || { select_full no_full_baseline; return 0; }
    for p in ${PARTS}; do
        cp="$(read_sha_file "${CI_ROOT}/part-success/${p}")" || { select_full "no_baseline_${p}"; return 0; }
        git merge-base --is-ancestor "${cp}" "${sha}" 2>/dev/null || { select_full "not_ancestor_${p}"; return 0; }
        if [ "${cp}" = "${sha}" ]; then part_base[${p}]="${cp}"; continue; fi
        files="$(git -c core.quotePath=false diff --no-renames --name-only "${cp}" "${sha}")" || { select_full diff_failed; return 0; }
        [ -n "${files}" ] || { select_full "empty_diff_${p}"; return 0; }
        out="$(printf '%s\n' "${files}" | bash scripts/ci-changed-parts.sh)" || { select_full classifier_failed; return 0; }
        for tok in ${out}; do
            case " ${PARTS} " in *" ${tok} "*) ;; *) select_full "classifier_${tok}"; return 0 ;; esac
        done
        case " ${out//$'\n'/ } " in
            *" ${p} "*) run_parts="${run_parts}${run_parts:+ }${p}" ;;
            *) part_base[${p}]="${cp}" ;;
        esac
    done
    if [ "${run_parts}" = "${PARTS}" ]; then select_full all_parts; return 0; fi
    if [ -n "${run_parts}" ]; then mode=scoped; select_reason=scoped; else mode=reuse; select_reason=reuse; fi
    return 0
}
if ! select_parts; then select_full select_failed; fi
if [ "${mode}" = docs_only ]; then docs_only=1; fi
in_run_parts() { case " ${run_parts} " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# 四段各自跑完再彙總：一段紅了（例如只在 Linux 紅的 ops 腳本）也照樣拿得到其他段的結果。
rc=0
failed=""
# 被訊號收掉的段落（timeout 124、KILL 137、TERM 143）：登出殺掉使用者行程、timeout 到了都會這樣，不是測試紅（#885）。
interrupted=""
signal_only=1
# 每段花的秒數（#1448）：log 每段結尾的 `secs=`，status.json 的 parts。只量，不改任何判斷。
# 沒跑的段記 null（不是 0：0 看起來像「測過、很快」）。
ob_secs=null
ops_secs=null
web_secs=null
daemon_secs=null
: > "${log}"
# 找到真正的 herdr binary 才啟用 argv parser 契約測試；PATH 上的 per-bot shell shim 不算。
real_herdr=""
for candidate in "${AM_REAL_HERDR:-}" "$(type -P herdr 2>/dev/null || true)"; do
    [ -n "${candidate}" ] || continue
    [ -f "${candidate}" ] && [ -x "${candidate}" ] || continue
    [ "$(head -c 2 "${candidate}" 2>/dev/null || true)" != '#!' ] || continue
    real_herdr="${candidate}"
    break
done
printf '==> [ubuntu-ci] mode=%s reason=%s run=[%s]\n' "${mode}" "${select_reason}" "${run_parts}" >> "${log}"
part_state_json=""
part_base_json=""
if [ "${docs_only}" = 0 ]; then
for part in ${PARTS}; do
    if ! in_run_parts "${part}"; then
        # 沒跑：不是「測過」。secs 記 null、status.json 標 reused 並附上沿用的基準。
        part_state_json="${part_state_json}${part_state_json:+,}\"${part}\":\"reused\""
        part_base_json="${part_base_json}${part_base_json:+,}\"${part}\":\"${part_base[${part}]:-}\""
        printf '\n==> [ubuntu-ci] %s reused（未執行；沿用基準 %s）\n' "${part}" "${part_base[${part}]:-}" >> "${log}"
        continue
    fi
    part_state_json="${part_state_json}${part_state_json:+,}\"${part}\":\"run\""
    part_base_json="${part_base_json}${part_base_json:+,}\"${part}\":\"\""
    prc=0
    bundle_failed=0
    part_start=${SECONDS}
    if [ "${part}" = daemon ] && [ ! -f web/dist/index.html ]; then
        # web 段沒跑（或沒跑到 build）：daemon 要嵌入的 bundle 由本輪 HEAD 重建。失敗＝daemon 這段紅，不能用舊 bundle 或 stub 放行。
        printf '\n==> [ubuntu-ci] web bundle for daemon（本輪 HEAD 重建）\n' >> "${log}"
        timeout -k "${KILL_AFTER}" "${TIMEOUT}" env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR -u AM_DAEMON_EXE -u AM_CONFIG_PATH \
            bash -c "${WEB_BUNDLE_CMD}" >> "${log}" 2>&1 || prc=$?
        [ "${prc}" = 0 ] || bundle_failed=1
    fi
    if [ "${bundle_failed}" = 0 ]; then
    timeout -k "${KILL_AFTER}" "${TIMEOUT}" env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR -u AM_DAEMON_EXE -u AM_CONFIG_PATH \
        bash scripts/check.sh "${part}" >> "${log}" 2>&1 || prc=$?
    fi
    if [ "${part}" = daemon ] && [ -n "${real_herdr}" ] && [ "${bundle_failed}" = 0 ]; then
        herdr_prc=0
        timeout -k "${KILL_AFTER}" "${TIMEOUT}" env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR -u AM_DAEMON_EXE -u AM_CONFIG_PATH \
            AM_TEST_REAL_HERDR=1 AM_REAL_HERDR="${real_herdr}" cargo test -p agents-managerd --locked -- \
            the_generated_argv_parses_with_the_real_herdr >> "${log}" 2>&1 || herdr_prc=$?
        printf '\n==> [ubuntu-ci] herdr argv parser rc=%s\n' "${herdr_prc}" >> "${log}"
        if [ "${herdr_prc}" != 0 ]; then prc="${herdr_prc}"; fi
    fi
    part_secs=$((SECONDS - part_start))
    printf '\n==> [ubuntu-ci] %s rc=%s secs=%s\n' "${part}" "${prc}" "${part_secs}" >> "${log}"
    case "${part}" in
        ob) ob_secs=${part_secs} ;;
        ops) ops_secs=${part_secs} ;;
        web) web_secs=${part_secs} ;;
        daemon) daemon_secs=${part_secs} ;;
    esac
    if [ "${prc}" = 0 ]; then
        # 這段的基準只在它本輪真的跑成功時前進（部分成功不影響其他段；紅的段留在舊基準，下個提交的累積差異還含著它）。
        write_sha_file "${CI_ROOT}/part-success/${part}" "${sha}" || echo "ubuntu-ci: 寫不下 ${part} 的成功基準，下一輪該段會全跑" >&2
    fi
    if [ "${prc}" != 0 ]; then
        rc="${prc}"
        failed="${failed}${failed:+,}${part}"
        case "${prc}" in
            124|137|143) interrupted="${interrupted}${interrupted:+,}${part}(rc=${prc})" ;;
            *) signal_only=0 ;;
        esac
    fi
done
else
    printf '==> [ubuntu-ci] docs-only skip (full success: %s)\n' "${full_success}" >> "${log}"
    for part in ${PARTS}; do
        part_state_json="${part_state_json}${part_state_json:+,}\"${part}\":\"reused\""
        part_base_json="${part_base_json}${part_base_json:+,}\"${part}\":\"${full_success}\""
    done
fi

finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
# parts＝每段秒數（沒跑＝null）；mode／full_baseline／part_state／part_base 說明這一輪哪些段真的跑、沒跑的沿用哪個基準。
parts_json="$(printf '"parts":{"ob":%s,"ops":%s,"web":%s,"daemon":%s},"mode":"%s","select_reason":"%s","full_baseline":"%s","part_state":{%s},"part_base":{%s}' \
    "${ob_secs}" "${ops_secs}" "${web_secs}" "${daemon_secs}" "${mode}" "$(json_str "${select_reason}")" "$(json_str "${full_success}")" "${part_state_json}" "${part_base_json}")"
# 中斷（#885）：有段落被訊號收掉、其餘紅的段也都是訊號收的，而且整份 log 沒有任何測試紅的證據（`test … FAILED`、`FAIL - `）
# → 不是這個 commit 的錯。記 error、不前進 last-sha，下一輪（每分鐘）同一個 sha 重跑；同一個 sha 連續中斷 3 次才當 failure，
# 免得一直被殺的環境永遠不出結果。計數檔 interrupted-<sha> 在正常結束（success／真的 failure）時刪掉，別的 sha 的殘檔順手清。
irc_file="${CI_ROOT}/interrupted-${sha}"
fail_prefix=""
for old_irc in "${CI_ROOT}"/interrupted-*; do
    [ -e "${old_irc}" ] && [ "${old_irc}" != "${irc_file}" ] && rm -f -- "${old_irc}"
done
# 磁碟滿（#945）：跑到一半 ENOSPC（同機的 worktree 把碟寫滿）是環境問題不是這個 commit 的錯。證據：log 有 `No space left on device`
# 或 check.sh 的「失敗時磁碟只剩」，或跑完再量一次剩餘空間已低於門檻。紅燈會讓 last-sha 前進、同一個 sha 不再重試，ci-watch 還會開票派工，
# 所以走跟中斷同一條路（記 error、不動 last-sha、共用 interrupted-<sha> 計數，3 次才當 failure）。ENOSPC 常讓測試也跟著紅，所以不看 FAIL 證據。
end_free_kb="$(df -Pk "${CI_ROOT}" 2>/dev/null | awk 'NR==2 {print $4}' || true)"
# 量不出來就不拿這條當證據（當成很大），免得 df 壞掉時每個紅燈都被說成磁碟滿。
case "${end_free_kb}" in *[!0-9]*|'') end_free_gb=999999 ;; *) end_free_gb=$(( end_free_kb / 1048576 )) ;; esac
disk_full=0
if [ "${rc}" != 0 ] && { grep -Fq 'No space left on device' "${log}" || grep -Fq '!! 失敗時磁碟只剩' "${log}" \
    || [ "${end_free_gb}" -lt "${MIN_FREE_GB}" ]; }; then
    disk_full=1
fi
if [ "${rc}" != 0 ] && { [ "${disk_full}" = 1 ] || { [ -n "${interrupted}" ] && [ "${signal_only}" = 1 ] \
    && ! grep -Eq '^test .* \.\.\. FAILED$|^FAIL - ' "${log}"; }; }; then
    irc_count="$(cat "${irc_file}" 2>/dev/null || echo 0)"
    case "${irc_count}" in *[!0-9]*|'') irc_count=0 ;; esac
    irc_count=$((irc_count + 1))
    if [ "${disk_full}" = 1 ]; then
        reason=disk_full
        fail_kind="連續磁碟滿 3 次："
    else
        reason=interrupted
        fail_kind="連續中斷 3 次："
    fi
    if [ "${irc_count}" -lt 3 ]; then
        echo "${irc_count}" > "${irc_file}"
        if [ "${disk_full}" = 1 ]; then
            desc="$(clip_chars 120 "$(printf '磁碟滿：跑完剩 %sG（門檻 %sG），會重跑' "${end_free_gb}" "${MIN_FREE_GB}")")"
        else
            desc="$(clip_chars 120 "$(printf '中斷：%s，會重跑' "${interrupted}")")"
        fi
        final_status error "${desc}"
        printf '{"sha":"%s","state":"error","reason":"%s","rc":%s,"interrupted_count":%s,"started":"%s","finished":"%s","log":"%s","description":"%s",%s}\n' \
            "${sha}" "${reason}" "${rc}" "${irc_count}" "${started}" "${finished}" "$(json_str "${log}")" "$(json_str "${desc}")" "${parts_json}" > "${CI_ROOT}/status.json"
        finalized=1
        exit 0
    fi
    fail_prefix="${fail_kind}"
fi
rm -f -- "${irc_file}"
# 完整四段「本輪」跑完（成功或失敗）就記時間：定期完整四段從這裡起算，失敗後不會每分鐘重跑。
if [ "${mode}" = full ]; then date +%s > "${CI_ROOT}/full-attempt-at" 2>/dev/null || true; fi
if [ "${rc}" = 0 ]; then
    state=success
    case "${mode}" in
        docs_only)
            desc="docs-only skip（沿用完整成功基準 ${full_success:0:8}）" ;;
        reuse)
            # 沒有任何段要跑，但完整基準到 HEAD 的差異含程式碼（各段在中間提交已驗）：不是 docs-only，用 scoped none。
            desc="scoped none（沿用完整成功基準 ${full_success:0:8}）" ;;
        scoped)
            # 契約（ci-pr 解析）：字面 `scoped` 前綴＋段落順序 ob/ops/web/daemon＋沿用的完整成功基準前 8 碼。
            desc="scoped ${run_parts// /+}（沿用完整成功基準 ${full_success:0:8}）" ;;
        *)
            # 只有四段本輪全跑成功才前進完整基準；部分成功不能冒充。
            desc="ob+ops+web+daemon 全綠"
            echo "${sha}" > "${CI_ROOT}/full-success-sha"
            date +%s > "${CI_ROOT}/full-success-at" 2>/dev/null || true ;;
    esac
else
    state=failure
    # 紅的段落＋第一個紅段死掉前的最後一個 step 標題；GitHub description 上限 140 字。
    first="$(awk '/^==> \[ubuntu-ci\] .* rc=[1-9]/ {print last; exit} /^==> / {last=substr($0, 5)}' "${log}")"
    # cargo test 紅了就點出是哪幾條（高負載偶發紅時，只寫「cargo test 紅了」看不出是誰）；編譯錯誤沒有這種行，照舊寫 step 標題。
    red_tests="$(grep -E '^test .* \.\.\. FAILED$' "${log}" | awk '{print $2}' || true)"
    if [ -n "${red_tests}" ]; then
        desc="$(clip_chars 120 "$(printf '紅：%s，%s 條測試紅（%s）' "${failed}" "$(printf '%s\n' "${red_tests}" | wc -l | tr -d ' ')" "$(printf '%s\n' "${red_tests}" | head -1)")")"
    else
        desc="$(clip_chars 120 "$(printf '紅：%s（%s）' "${failed}" "${first}")")"
    fi
    desc="${fail_prefix}${desc}"
fi
final_status "${state}" "${desc}"
printf '{"sha":"%s","state":"%s","rc":%s,"started":"%s","finished":"%s","log":"%s","description":"%s",%s}\n' \
    "${sha}" "${state}" "${rc}" "${started}" "${finished}" "$(json_str "${log}")" "$(json_str "${desc}")" "${parts_json}" > "${CI_ROOT}/status.json"
echo "${sha}" > "${CI_ROOT}/last-sha"
finalized=1

# 只留最近幾份 log。
ls -1t "${CI_ROOT}/logs"/*.log 2>/dev/null | tail -n "+$((KEEP_LOGS + 1))" | while IFS= read -r old; do
    rm -f -- "${old}"
done
