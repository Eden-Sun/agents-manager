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
sha="$(git rev-parse origin/main)"
last="$(cat "${CI_ROOT}/last-sha" 2>/dev/null || true)"
if [ "${sha}" = "${last}" ]; then
    exit 0
fi

status() {
    # 回報失敗不能讓整輪中斷：GitHub 暫時連不上時，結果仍寫在 status.json。
    gh api -X POST "repos/${GH_REPO}/statuses/${sha}" -f state="$1" -f context="${CONTEXT}" \
        -f description="$2" >/dev/null 2>&1 || echo "commit status 寫不上去（$1）" >&2
}

log="${CI_ROOT}/logs/${sha}.log"
started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
printf '{"sha":"%s","state":"running","started":"%s","log":"%s"}\n' "${sha}" "${started}" "${log}" > "${CI_ROOT}/status.json"
status pending "ubuntu 完整 CI 執行中"

# pending 送出之後任何一步（checkout、clean、彙總）因 set -e 中斷，都不能讓 commit status 永遠停在 pending、
# status.json 永遠停在 running：離開時還沒寫完結果就補一個 error（last-sha 不動，下一輪會重試）。
finalized=0
on_exit() {
    local code=$?
    if [ "${finalized}" = 0 ]; then
        status error "ubuntu-ci 腳本中斷（rc=${code}），見 journal"
        printf '{"sha":"%s","state":"error","rc":%s,"started":"%s","log":"%s"}\n' \
            "${sha}" "${code}" "${started}" "${log}" > "${CI_ROOT}/status.json"
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
    desc="$(printf '紅：%s（%s）' "${failed}" "${first}" | cut -c1-120)"
fi
status "${state}" "${desc}"
printf '{"sha":"%s","state":"%s","rc":%s,"started":"%s","finished":"%s","log":"%s","description":"%s"}\n' \
    "${sha}" "${state}" "${rc}" "${started}" "${finished}" "${log}" "${desc//\"/\'}" > "${CI_ROOT}/status.json"
echo "${sha}" > "${CI_ROOT}/last-sha"
finalized=1

# 只留最近幾份 log。
ls -1t "${CI_ROOT}/logs"/*.log 2>/dev/null | tail -n "+$((KEEP_LOGS + 1))" | while IFS= read -r old; do
    rm -f -- "${old}"
done
