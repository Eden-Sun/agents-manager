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

git checkout -q --detach -f "${sha}"
git clean -q -fdx -e target -e web/node_modules

rc=0
timeout "${TIMEOUT}" env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR -u AM_DAEMON_EXE -u AM_CONFIG_PATH \
    bash scripts/check.sh > "${log}" 2>&1 || rc=$?

finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [ "${rc}" = 0 ]; then
    state=success
    desc="ob+ops+web+daemon 全綠"
else
    state=failure
    # 最後一個 step 標題＝死在哪一段；GitHub description 上限 140 字。
    step_line="$(grep -E '^==> ' "${log}" | tail -1 | sed 's/^==> //')"
    desc="$(printf 'rc=%s 停在：%s' "${rc}" "${step_line}" | cut -c1-120)"
fi
status "${state}" "${desc}"
printf '{"sha":"%s","state":"%s","rc":%s,"started":"%s","finished":"%s","log":"%s","description":"%s"}\n' \
    "${sha}" "${state}" "${rc}" "${started}" "${finished}" "${log}" "${desc//\"/\'}" > "${CI_ROOT}/status.json"
echo "${sha}" > "${CI_ROOT}/last-sha"

# 只留最近幾份 log。
ls -1t "${CI_ROOT}/logs"/*.log 2>/dev/null | tail -n "+$((KEEP_LOGS + 1))" | while IFS= read -r old; do
    rm -f -- "${old}"
done
