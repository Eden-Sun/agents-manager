#!/usr/bin/env bash
# ubuntu 背景完整 CI（issue #716）：取代「每個 push 都等 GitHub Actions」。
#
# 由 systemd user timer 每分鐘叫一次（scripts/ops/ci/ubuntu-ci.{service,timer}）。每次：
#   1. 拿不到鎖＝上一輪還在跑，直接退出（同時最多 1 個 running）。
#   2. fetch origin/main；跟上次驗過的 sha 一樣就退出。
#   3. 只驗「現在最新的」main HEAD（latest-HEAD coalescing：中間被跳過的 sha 不補跑）。
#   4. 在常駐 clone 裡 checkout 該 sha、保留 target／node_modules 當暖快取，跑 scripts/check.sh（ob、ops、web、daemon）。
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

mkdir -p "${CI_ROOT}/logs"
exec 9>"${CI_ROOT}/lock"
if ! flock -n 9; then
    exit 0
fi

if [ ! -d "${CI_ROOT}/repo/.git" ]; then
    git clone -q "${REPO_URL}" "${CI_ROOT}/repo"
fi
cd "${CI_ROOT}/repo"
git fetch -q origin main

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

# 上一輪沒送上去的最後結果（見 final_status）先補送：要在「同一個 sha 就退出」之前，不然 main 靜止時永遠補不到。
post_unposted() {
    local usha ustate udesc
    [ -s "${CI_ROOT}/unposted" ] || return 0
    IFS=$'\t' read -r usha ustate udesc < "${CI_ROOT}/unposted" || true
    [ -n "${usha}" ] && [ -n "${ustate}" ] || { rm -f "${CI_ROOT}/unposted"; return 0; }
    if post_status "${usha}" "${ustate}" "${udesc}"; then
        rm -f "${CI_ROOT}/unposted"
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

sha="$(git rev-parse origin/main)"
last="$(cat "${CI_ROOT}/last-sha" 2>/dev/null || true)"
if [ "${sha}" = "${last}" ]; then
    exit 0
fi

# 回報失敗不能讓整輪中斷：GitHub 暫時連不上時，結果仍寫在 status.json。
status() {
    post_status "${sha}" "$1" "$2" || true
}

# 最後的結果（success／failure／error）一定要送到：送不上去就記在 ${CI_ROOT}/unposted（一行：sha、state、description，tab 分隔），
# 下一輪（不管有沒有新 commit、也不重跑檢查）先補送。不然 last-sha 已經前進，這個 sha 在 GitHub 上永遠停在 pending，
# 而 status.json 卻寫著 success。
final_status() {
    if ! post_status "${sha}" "$1" "$2"; then
        printf '%s\t%s\t%s\n' "${sha}" "$1" "$2" > "${CI_ROOT}/unposted"
    fi
}


# 放進手寫 JSON 字串前的跳脫：反斜線、雙引號、控制字元（description 取自 step 標題，什麼字元都可能有）。
json_str() {
    local v="$1"
    v="${v//\\/\\\\}"
    v="${v//\"/\\\"}"
    printf '%s' "${v}" | LC_ALL=C tr -d '\000-\037'
}

# 保險（#763）：舊版測試與被 kill 的測試行程留下的 am-*／agm-* 暫存目錄／檔案（名字結尾是 26 碼 ULID）會慢慢塞滿根磁碟
# （2026-10-01 實測 44G、ubuntu-ci 因 ENOSPC 紅 19 條）。持有鎖時才清（ubuntu-ci 自己那一輪不會被誤刪），失敗一律吞掉。保守規則：
#   - 排在磁碟預檢**之前**：磁碟被這些東西塞滿的那一輪正是需要清的那一輪，排在後面就永遠清不到。
#   - 只認 ${TMPDIR}（沒設用 /tmp）底下第一層；${TMPDIR} 不是絕對路徑、不存在、或解析後是 / 或 ${HOME} 就整個不動（設錯不能照字面去刪）。
#   - 只處理真的目錄與一般檔案（find 預設不跟 symlink，-type d／f 也排除 symlink 本身），rm 不會跟進 symlink 刪到別處。
#   - 超過 6 小時沒動：目錄自己的 mtime 只在直接子項增減時才變，所以裡面還有 6 小時內動過的東西就跳過（有行程可能還在用）。
#   - 名字必須以 am- 或 agm- 開頭、並以 26 碼 ULID（可再接一個 .副檔名）結尾；沒有 ULID 的 am-*（如 am-ops-test）、herdr-*、claude-* 都不碰。
# 僅 Linux（GNU find 的 -regextype；整支腳本本來就用 flock／timeout）：BSD find 不認得它時整段失敗被吞掉，等於不清、不會誤刪。
sweep_stale_tmp() {
    local root real home_real d
    root="${TMPDIR:-/tmp}"
    case "${root}" in /*) ;; *) return 0 ;; esac
    real="$(cd "${root}" 2>/dev/null && pwd -P)" || return 0
    home_real="$(cd "${HOME:-/nonexistent}" 2>/dev/null && pwd -P || true)"
    case "${real}" in /|"${home_real}") return 0 ;; esac
    while IFS= read -r -d '' d; do
        if [ -d "${d}" ] && [ -n "$(find "${d}" -mmin -360 -print -quit 2>/dev/null)" ]; then
            continue
        fi
        rm -rf -- "${d}" 2>/dev/null || true
    done < <(find "${real}" -mindepth 1 -maxdepth 1 \( -type d -o -type f \) -mmin +360 -regextype posix-extended \
        -regex '.*/(am|agm)-[A-Za-z0-9-]*[0-9A-HJKMNP-TV-Z]{26}(\.[a-z]+)?' -print0 2>/dev/null || true)
}
sweep_stale_tmp || true

# 磁碟預檢（在任何 checkout／clean／建置之前）：不足就寫 error、不前進 last-sha，空間恢復後同一個 sha 會重跑。
# 每分鐘都會再進來，所以同一個 sha 只送一次 status（disk-low-sha 記著）。
free_kb="$(df -Pk "${CI_ROOT}" | awk 'NR==2 {print $4}')"
free_gb=$(( ${free_kb:-0} / 1048576 ))
if [ "${free_gb}" -lt "${MIN_FREE_GB}" ]; then
    if [ "$(cat "${CI_ROOT}/disk-low-sha" 2>/dev/null || true)" != "${sha}" ]; then
        status error "磁碟不足，未執行（剩 ${free_gb}G，門檻 ${MIN_FREE_GB}G）"
        printf '{"sha":"%s","state":"error","reason":"disk_low","free_gb":%s,"min_free_gb":%s,"at":"%s"}\n' \
            "${sha}" "${free_gb}" "${MIN_FREE_GB}" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "${CI_ROOT}/status.json"
        echo "${sha}" > "${CI_ROOT}/disk-low-sha"
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

# 四段各自跑完再彙總：一段紅了（例如只在 Linux 紅的 ops 腳本）也照樣拿得到其他段的結果。
rc=0
failed=""
: > "${log}"
for part in ob ops web daemon; do
    prc=0
    timeout -k "${KILL_AFTER}" "${TIMEOUT}" env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR -u AM_DAEMON_EXE -u AM_CONFIG_PATH \
        bash scripts/check.sh "${part}" >> "${log}" 2>&1 || prc=$?
    printf '\n==> [ubuntu-ci] %s rc=%s\n' "${part}" "${prc}" >> "${log}"
    if [ "${prc}" != 0 ]; then
        rc="${prc}"
        failed="${failed}${failed:+,}${part}"
    fi
done

finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [ "${rc}" = 0 ]; then
    state=success
    desc="ob+ops+web+daemon 全綠"
else
    state=failure
    # 紅的段落＋第一個紅段死掉前的最後一個 step 標題；GitHub description 上限 140 字。
    first="$(awk '/^==> \[ubuntu-ci\] .* rc=[1-9]/ {print last; exit} /^==> / {last=substr($0, 5)}' "${log}")"
    # cargo test 紅了就點出是哪幾條（高負載偶發紅時，只寫「cargo test 紅了」看不出是誰）；編譯錯誤沒有這種行，照舊寫 step 標題。
    red_tests="$(grep -E '^test .* \.\.\. FAILED$' "${log}" | awk '{print $2}' || true)"
    if [ -n "${red_tests}" ]; then
        desc="$(printf '紅：%s，%s 條測試紅（%s）' "${failed}" "$(printf '%s\n' "${red_tests}" | wc -l | tr -d ' ')" "$(printf '%s\n' "${red_tests}" | head -1)" | cut -c1-120)"
    else
        desc="$(printf '紅：%s（%s）' "${failed}" "${first}" | cut -c1-120)"
    fi
fi
final_status "${state}" "${desc}"
printf '{"sha":"%s","state":"%s","rc":%s,"started":"%s","finished":"%s","log":"%s","description":"%s"}\n' \
    "${sha}" "${state}" "${rc}" "${started}" "${finished}" "$(json_str "${log}")" "$(json_str "${desc}")" > "${CI_ROOT}/status.json"
echo "${sha}" > "${CI_ROOT}/last-sha"
finalized=1

# 只留最近幾份 log。
ls -1t "${CI_ROOT}/logs"/*.log 2>/dev/null | tail -n "+$((KEEP_LOGS + 1))" | while IFS= read -r old; do
    rm -f -- "${old}"
done
