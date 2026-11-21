#!/usr/bin/env bash
set -euo pipefail

export AGM_RUST_ENGINE=cargo

CI_ROOT="${AGM_CI_ROOT:-${HOME}/.cache/agents-manager/ci}"
REPO_URL="${AGM_CI_REPO_URL:-git@github.com:Eden-Sun/agents-manager.git}"
GH_REPO="${AGM_CI_GH_REPO:-Eden-Sun/agents-manager}"
CONTEXT="ubuntu-ci"
TIMEOUT="${AGM_CI_TIMEOUT:-45m}"
KILL_AFTER="${AGM_CI_KILL_AFTER:-60s}"
KEEP_LOGS="${AGM_CI_KEEP_LOGS:-30}"
MIN_FREE_GB="${AGM_CI_MIN_FREE_GB:-20}"
OPS_SYNC_INTERVAL="${AGM_CI_OPS_SYNC_INTERVAL:-21600}"
FULL_INTERVAL="${AGM_CI_FULL_INTERVAL:-86400}"
case "${FULL_INTERVAL}" in
    ''|*[!0-9]*)
        echo "ubuntu-ci: AGM_CI_FULL_INTERVAL='${FULL_INTERVAL}' 不是非負整數，改用預設 86400" >&2
        FULL_INTERVAL=86400 ;;
esac
WEB_BUNDLE_CMD="${AGM_CI_WEB_BUNDLE_CMD:-cd web && bun install --frozen-lockfile && bunx vite build}"
PARTS="ob ops web daemon"

DEPLOY_DIR="${AGM_CI_DEPLOY_DIR:-${AGM_DIR:-${HOME}/.config/agents-manager/supervisor/AGM}}"
DEPLOY_WAIT="${AGM_CI_DEPLOY_WAIT:-1800}"
DEPLOY_POLL="${AGM_CI_DEPLOY_POLL:-10}"
case "${DEPLOY_WAIT}" in ''|*[!0-9]*) echo "ubuntu-ci: AGM_CI_DEPLOY_WAIT='${DEPLOY_WAIT}' 不是非負整數，改用預設 1800" >&2; DEPLOY_WAIT=1800 ;; esac
case "${DEPLOY_POLL}" in ''|*[!0-9]*|0) DEPLOY_POLL=10 ;; esac
deploy_lock_held() {
    local lock="${DEPLOY_DIR}/daemon-update.lock" pid
    [ -d "${lock}" ] || return 1
    pid=$(cut -d' ' -f1 "${lock}/owner" 2>/dev/null || true)
    case "${pid}" in ''|*[!0-9]*) return 1 ;; esac
    kill -0 "${pid}" 2>/dev/null
}
yield_to_deploy() {
    local out="$1" waited=0
    [ "${DEPLOY_WAIT}" -gt 0 ] || return 0
    deploy_lock_held || return 0
    printf '[ubuntu-ci] 部署正在建置／換版（%s/daemon-update.lock），CI 讓路，最多等 %ss\n' "${DEPLOY_DIR}" "${DEPLOY_WAIT}" | tee -a "${out}" >&2
    while deploy_lock_held; do
        if [ "${waited}" -ge "${DEPLOY_WAIT}" ]; then
            printf '[ubuntu-ci] 等了 %ss 部署還沒結束，不再讓路，照跑\n' "${waited}" | tee -a "${out}" >&2
            return 0
        fi
        sleep "${DEPLOY_POLL}"
        waited=$((waited + DEPLOY_POLL))
    done
    printf '[ubuntu-ci] 部署已結束（等了 %ss），開始跑\n' "${waited}" | tee -a "${out}" >&2
}

GIT_BIN="${AGM_CI_GIT_BIN:-git}"
FETCH_TIMEOUT="${AGM_CI_FETCH_TIMEOUT:-300}"
CLONE_TIMEOUT="${AGM_CI_CLONE_TIMEOUT:-600}"
HUNG_AFTER="${AGM_CI_HUNG_AFTER:-11700}"

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
date +%s > "${CI_ROOT}/running-since" 2>/dev/null || true

if [ ! -d "${CI_ROOT}/repo/.git" ]; then
    timeout -k 10 "${CLONE_TIMEOUT}" "${GIT_BIN}" clone -q "${REPO_URL}" "${CI_ROOT}/repo"
fi
cd "${CI_ROOT}/repo"
timeout -k 10 "${FETCH_TIMEOUT}" "${GIT_BIN}" fetch -q origin main

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

ops_sync_check() {
    local now last rc=0
    [ "${OPS_SYNC_INTERVAL}" -gt 0 ] 2>/dev/null || return 0
    now="$(date +%s)"
    last="$(cat "${CI_ROOT}/ops-sync.last" 2>/dev/null || echo 0)"
    [ $((now - last)) -ge "${OPS_SYNC_INTERVAL}" ] || return 0
    echo "${now}" > "${CI_ROOT}/ops-sync.last"
    timeout -k 10 120 python3 -B "${CI_ROOT}/repo/scripts/agm.py" ops-sync --check --alert --repo "${CI_ROOT}/repo" \
        > "${CI_ROOT}/ops-sync.json" 2> "${CI_ROOT}/ops-sync.err" || rc=$?
    [ "${rc}" -le 1 ] || echo "ubuntu-ci: ops-sync 檢查沒跑成（rc=${rc}），見 ${CI_ROOT}/ops-sync.err" >&2
    return 0
}
ops_sync_check || true

full_due() {
    local now t f latest=0
    [ "${FULL_INTERVAL}" -gt 0 ] || return 1
    now="$(date +%s)"
    for f in full-success-at full-attempt-at; do
        t="$(cat "${CI_ROOT}/${f}" 2>/dev/null || echo 0)"
        case "${t}" in *[!0-9]*|''|0[0-9]*) t=0 ;; esac
        [ "${#t}" -le 12 ] || t=0
        t=$((10#${t}))
        [ "${t}" -le "${now}" ] || t=0
        [ "${t}" -le "${latest}" ] || latest="${t}"
    done
    if [ "${latest}" = 0 ] && [ -f "${CI_ROOT}/full-success-sha" ]; then
        latest="$(stat -c %Y "${CI_ROOT}/full-success-sha" 2>/dev/null || echo 0)"
        case "${latest}" in *[!0-9]*|''|0[0-9]*) latest=0 ;; esac
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

status() {
    post_status "${sha}" "$1" "$2" || true
}

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
        if ! rewrite_unposted_sha "${sha}" remove; then
            rewrite_unposted_sha "${sha}" write "$1" "$2"
        fi
    else
        rewrite_unposted_sha "${sha}" write "$1" "$2"
    fi
}

json_str() {
    local v="$1"
    v="${v//\\/\\\\}"
    v="${v//\"/\\\"}"
    printf '%s' "${v}" | LC_ALL=C tr -d '\000-\037'
}

clip_chars() {
    python3 -X utf8 -c 'import sys; sys.stdout.write(sys.argv[2][:int(sys.argv[1])])' "$1" "$2" 2>/dev/null || printf '%s' "$2"
}

sweep_stale_tmp() {
    local root logical real home_real d
    root="${TMPDIR:-/tmp}"
    case "${root}" in /*) ;; *) return 0 ;; esac
    logical="$(cd "${root}" 2>/dev/null && pwd -L)" || return 0
    real="$(cd "${root}" 2>/dev/null && pwd -P)" || return 0
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

free_kb="$(df -Pk "${CI_ROOT}" | awk 'NR==2 {print $4}')"
free_gb=$(( ${free_kb:-0} / 1048576 ))
if [ "${free_gb}" -lt "${MIN_FREE_GB}" ]; then
    if [ "$(cat "${CI_ROOT}/disk-low-sha" 2>/dev/null || true)" != "${sha}" ]; then
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
write_sha_file() {
    local tmp
    mkdir -p "$(dirname "$1")" 2>/dev/null || return 1
    tmp="$(mktemp "$1.XXXXXX")" || return 1
    if printf '%s\n' "$2" > "${tmp}" && mv -f -- "${tmp}" "$1"; then return 0; fi
    rm -f -- "${tmp}"
    return 1
}

has_pending_parts() {
    local p
    for p in ${PARTS}; do
        if [ -e "${CI_ROOT}/part-pending/${p}" ] || [ -L "${CI_ROOT}/part-pending/${p}" ]; then return 0; fi
    done
    return 1
}

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

declare -A part_state part_base
run_parts=""
mode=full
select_reason=""
select_full() { mode=full; select_reason="$1"; run_parts="${PARTS}"; }
select_parts() {
    local p cp files out tok
    run_parts=""
    if [ "${force_full}" = 1 ]; then select_full periodic; return 0; fi
    read_sha_file "${CI_ROOT}/part-outcomes-v1" >/dev/null || { select_full no_outcome_state; return 0; }
    if ! has_pending_parts && docs_only_since_full_success; then mode=docs_only; select_reason=docs_only; return 0; fi
    is_sha "${full_success}" && git merge-base --is-ancestor "${full_success}" "${sha}" 2>/dev/null \
        || { select_full no_full_baseline; return 0; }
    for p in ${PARTS}; do
        cp="$(read_sha_file "${CI_ROOT}/part-success/${p}")" || { select_full "no_baseline_${p}"; return 0; }
        git merge-base --is-ancestor "${cp}" "${sha}" 2>/dev/null || { select_full "not_ancestor_${p}"; return 0; }
        if [ -e "${CI_ROOT}/part-pending/${p}" ] || [ -L "${CI_ROOT}/part-pending/${p}" ]; then
            run_parts="${run_parts}${run_parts:+ }${p}"
            continue
        fi
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

rc=0
failed=""
interrupted=""
signal_only=1
ob_secs=null
ops_secs=null
web_secs=null
daemon_secs=null
: > "${log}"
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
yield_to_deploy "${log}"
for part in ${PARTS}; do
    if ! in_run_parts "${part}"; then
        part_state_json="${part_state_json}${part_state_json:+,}\"${part}\":\"reused\""
        part_base_json="${part_base_json}${part_base_json:+,}\"${part}\":\"${part_base[${part}]:-}\""
        printf '\n==> [ubuntu-ci] %s reused（未執行；沿用基準 %s）\n' "${part}" "${part_base[${part}]:-}" >> "${log}"
        continue
    fi
    part_state_json="${part_state_json}${part_state_json:+,}\"${part}\":\"run\""
    part_base_json="${part_base_json}${part_base_json:+,}\"${part}\":\"\""
    write_sha_file "${CI_ROOT}/part-pending/${part}" "${sha}"
    prc=0
    bundle_failed=0
    part_start=${SECONDS}
    if [ "${part}" = daemon ] && [ ! -f web/dist/index.html ]; then
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
        main_prc=${prc}
        printf '\n==> [ubuntu-ci] daemon primary check rc=%s\n' "${main_prc}" >> "${log}"
        herdr_prc=0
        timeout -k "${KILL_AFTER}" "${TIMEOUT}" env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR -u AM_DAEMON_EXE -u AM_CONFIG_PATH \
            AM_TEST_REAL_HERDR=1 AM_REAL_HERDR="${real_herdr}" cargo test -p agents-managerd --locked -- \
            the_generated_argv_parses_with_the_real_herdr >> "${log}" 2>&1 || herdr_prc=$?
        case "${herdr_prc}" in
            0) herdr_result=success ;;
            124|137|143) herdr_result=interrupted ;;
            *) herdr_result=failure ;;
        esac
        printf '\n==> [ubuntu-ci] herdr argv parser rc=%s result=%s\n' "${herdr_prc}" "${herdr_result}" >> "${log}"
        if [ "${herdr_prc}" != 0 ]; then
            case "${prc}" in
                0) prc="${herdr_prc}" ;;
                124|137|143)
                    case "${herdr_prc}" in 124|137|143) ;; *) prc="${herdr_prc}" ;; esac ;;
                *) ;;
            esac
        fi
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
        if write_sha_file "${CI_ROOT}/part-success/${part}" "${sha}"; then
            rm -f -- "${CI_ROOT}/part-pending/${part}"
        else
            echo "ubuntu-ci: 寫不下 ${part} 的成功基準，保留待驗狀態" >&2
        fi
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
if [ "${mode}" = full ]; then
    write_sha_file "${CI_ROOT}/part-outcomes-v1" "${sha}"
fi
else
    printf '==> [ubuntu-ci] docs-only skip (full success: %s)\n' "${full_success}" >> "${log}"
    for part in ${PARTS}; do
        part_state_json="${part_state_json}${part_state_json:+,}\"${part}\":\"reused\""
        part_base_json="${part_base_json}${part_base_json:+,}\"${part}\":\"${full_success}\""
    done
fi

finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
parts_json="$(printf '"parts":{"ob":%s,"ops":%s,"web":%s,"daemon":%s},"mode":"%s","select_reason":"%s","full_baseline":"%s","part_state":{%s},"part_base":{%s}' \
    "${ob_secs}" "${ops_secs}" "${web_secs}" "${daemon_secs}" "${mode}" "$(json_str "${select_reason}")" "$(json_str "${full_success}")" "${part_state_json}" "${part_base_json}")"
irc_file="${CI_ROOT}/interrupted-${sha}"
fail_prefix=""
for old_irc in "${CI_ROOT}"/interrupted-*; do
    [ -e "${old_irc}" ] && [ "${old_irc}" != "${irc_file}" ] && rm -f -- "${old_irc}"
done
end_free_kb="$(df -Pk "${CI_ROOT}" 2>/dev/null | awk 'NR==2 {print $4}' || true)"
case "${end_free_kb}" in *[!0-9]*|'') end_free_gb=999999 ;; *) end_free_gb=$(( end_free_kb / 1048576 )) ;; esac
disk_full=0
if [ "${rc}" != 0 ] && { grep -Fq 'No space left on device' "${log}" || grep -Fq '!! 失敗時磁碟只剩' "${log}" \
    || [ "${end_free_gb}" -lt "${MIN_FREE_GB}" ]; }; then
    disk_full=1
fi
if [ "${rc}" != 0 ] && { [ "${disk_full}" = 1 ] || { [ -n "${interrupted}" ] && [ "${signal_only}" = 1 ] \
    && ! grep -Eq '^test .* \.\.\. FAILED$|^FAIL - ' "${log}"; }; }; then
    irc_count="$(cat "${irc_file}" 2>/dev/null || echo 0)"
    case "${irc_count}" in *[!0-9]*|''|0[0-9]*) irc_count=0 ;; esac
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
if [ "${mode}" = full ]; then date +%s > "${CI_ROOT}/full-attempt-at" 2>/dev/null || true; fi
if [ "${rc}" = 0 ]; then
    state=success
    case "${mode}" in
        docs_only)
            desc="docs-only skip（沿用完整成功基準 ${full_success:0:8}）" ;;
        reuse)
            desc="scoped none（沿用完整成功基準 ${full_success:0:8}）" ;;
        scoped)
            desc="scoped ${run_parts// /+}（沿用完整成功基準 ${full_success:0:8}）" ;;
        *)
            desc="ob+ops+web+daemon 全綠"
            echo "${sha}" > "${CI_ROOT}/full-success-sha"
            date +%s > "${CI_ROOT}/full-success-at" 2>/dev/null || true ;;
    esac
else
    state=failure
    first="$(awk '/^==> \[ubuntu-ci\] .* rc=[1-9]/ {print last; exit} /^==> / {last=substr($0, 5)}' "${log}")"
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

ls -1t "${CI_ROOT}/logs"/*.log 2>/dev/null | tail -n "+$((KEEP_LOGS + 1))" | while IFS= read -r old; do
    rm -f -- "${old}"
done
