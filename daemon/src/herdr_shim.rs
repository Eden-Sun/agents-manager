//! The `herdr` PATH shim (SPEC §6.5b): enforces `<parent>-<suffix>` child names (agents forget
//! the persona rule) and passes account/hook env down, since herdr's *server* spawns panes and
//! a child inherits nothing.

use std::path::{Path, PathBuf};

pub const SHIM_SH: &str = r##"#!/bin/sh
# agents-manager herdr shim (SPEC §6.5b). Installed at the front of a managed pane's PATH.
#
# Naming a child agent `<parent>-<suffix>`, and handing a child pane the account and hook
# environment its parent runs under, used to be a *request* written into the agent's persona.
# Here they are a mechanism: whatever the agent types, the child comes out named and carrying
# the env that makes it trackable.
#
# POSIX sh only — a pane's shell may be zsh, bash, dash or ash — and no `set -e`: a shim that
# aborts is worse than one that forwards.

# The real herdr: `$AM_REAL_HERDR` if the daemon resolved one, else the first `herdr` on PATH
# that is not this directory (otherwise we would exec ourselves forever).
am_real_herdr() {
    if [ -n "${AM_REAL_HERDR:-}" ] && [ -x "$AM_REAL_HERDR" ]; then
        printf '%s\n' "$AM_REAL_HERDR"
        return 0
    fi
    _self=$(cd "$(dirname "$0")" 2>/dev/null && pwd)
    printf '%s\n' "$PATH" | tr ':' '\n' | {
        while IFS= read -r _d; do
            [ -n "$_d" ] || _d=.
            _abs=$(cd "$_d" 2>/dev/null && pwd) || continue
            [ "$_abs" = "$_self" ] && continue
            if [ -x "$_abs/herdr" ]; then
                printf '%s\n' "$_abs/herdr"
                break
            fi
        done
    }
}

# PATH 去重，保留第一次出現的順序（#389）：每一代子 agent 把母代的 PATH 往下傳、外掛又補一次，一代比一代長。
am_dedupe_path() {
    printf '%s' "$1" | tr ':' '\n' | awk '!seen[$0]++' | paste -sd: -
}

# 帳號／hook 的保留清單：pane split／tab create／workspace create 補 `--env` 用（am_forward_with_env），
# agent start 沒有 `--env` 時補 export 用（am_reexport_env_before_start，issue #57）。單一清單，兩邊不會走鐘。
#
# `AM_DAEMON_EXE`／`AM_CONFIG_PATH`（issue #138）：cargo shim 把 check／test／clippy 轉到外部編譯主機的前提。
# 少了它們，子 agent 的 cargo 永遠留在本機——#104 當初只在 daemon 注入端加了，這份清單漏了。
# 跟其他 key 一樣：母 pane 有才帶、呼叫者自己給了就尊重（它們不是隔離實例那種要防偽造的保留變數）。
AM_RESERVED_ENV_KEYS="CLAUDE_CONFIG_DIR CODEX_HOME AM_BOT_ID AM_BOT_TOKEN AM_HOOK_TOKEN AM_PORT AM_RUN_ID AM_AGENT_NAME AM_KIND AM_MODEL AM_EFFORT AM_PROJECT_ID AM_WORKSPACE_ID AM_OUTBOX AM_DAEMON_EXE AM_CONFIG_PATH AM_REAL_HERDR PATH CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION CLAUDE_CODE_DISABLE_CLAUDE_MDS AM_INSTRUCTIONS_FILE"

# API calls use the independent bot credential. Keep the hook-token fallback for already-running
# hook-enabled panes until they restart to receive AM_BOT_TOKEN.
am_bot_token() {
    if [ -n "${AM_BOT_TOKEN:-}" ]; then printf '%s' "$AM_BOT_TOKEN"; else printf '%s' "${AM_HOOK_TOKEN:-}"; fi
}

# 子 agent 的標記（使用者 2026-09-30）：bot 開出來的 pane 一律帶 `AM_CHILD_OF=<母 agent 名>`，
# 子代自己再開的 pane 沿用同一個值（不往下疊）。有這個值的 pane 就是子 agent，`agent start` 直接拒絕：
# 子 agent 不再有子 agent，要人手由 parent 決定另派兄弟，狀態才都掛在同一層被追蹤。
# 呼叫者自帶的 `--env AM_CHILD_OF=…` 一律剝掉（清掉它就能繞過），只認這裡算出來的值。
am_child_of_value() {
    if [ -n "${AM_CHILD_OF:-}" ]; then
        printf '%s' "$AM_CHILD_OF"
    elif [ -n "${AM_BOT_ID:-}" ]; then
        printf '%s' "${AM_AGENT_NAME:-${AM_BOT_ID}}"
    fi
}

# 檔案內容 → TOML 多行字面字串（三個單引號包起來），給 codex 的 `developer_instructions`（§6.5i）。
# 字面字串不跳脫任何字元，碰不到各家 awk／sed 的反斜線差異；內容本身有三個連續單引號就表示不了，回 1 讓呼叫端不帶。
am_toml_string_of_file() {
    _q3="'''"
    if grep -qF "$_q3" "$1" 2>/dev/null; then
        return 1
    fi
    printf '%s%s%s' "$_q3" "$(cat "$1")" "$_q3"
}

# codex 沒有「從檔案讀 developer_instructions」的參數；`-c developer_instructions=<內容>` 是多行，herdr ≥0.9.0
# 的 `agent start` 擋所有控制字元（invalid_agent_argument，#772），壓成一行又會撞到打字長度上限（herdr.rs 的
# `fit_command_line`）。改寫成 `$CODEX_HOME/<名>.config.toml`，argv 只帶 `-p <名>`；印出 profile 名，失敗回 1。
# 每顆 bot 一個檔（同帳號的 bot 共用 CODEX_HOME），暫存檔＋mv 換上，兩個 child 同時開也不會讀到半份。
am_codex_instructions_profile() {
    _toml=$(am_toml_string_of_file "$1") || return 1
    _home=${CODEX_HOME:-$HOME/.codex}
    _name="am-child-$(printf '%s' "${AM_BOT_ID:-${AM_AGENT_NAME:-local}}" | tr -c 'A-Za-z0-9_-' '_')"
    _dst="$_home/$_name.config.toml"
    mkdir -p "$_home" 2>/dev/null
    if printf 'developer_instructions = %s\n' "$_toml" > "$_dst.$$" 2>/dev/null && mv -f "$_dst.$$" "$_dst"; then
        printf '%s' "$_name"
        return 0
    fi
    rm -f "$_dst.$$"
    return 1
}

# permit 的有效期要涵蓋 `agent start --timeout`（herdr 最多等 300 秒，預設 30 秒；子 agent 慢慢啟動時不能在 finish 之前過期）。
# `_AM_SPAWN_TIMEOUT_MS` 由 am_agent_start 解析出來；其他子命令沒有，daemon 用預設有效期。
_AM_SPAWN_TIMEOUT_MS=""

# A bot pane must reserve its inherited proof before asking herdr to create a pane or child agent.
# Rotation raises the matching daemon fence before its descendant snapshot; an unreadable / refused
# gate means we do not run herdr. The permit remains open until the created pane is registered.
#
# 遠端主機上的 bot（有 bot 身分、沒有 `AM_PORT`）不走 fence：遠端沒有 daemon、不開反向埠（SPEC §11.4），
# daemon 本來就不注入 `AM_PORT`，跟 cargo shim 的 #153 同一條判準。以前這裡把它當成「確認不了」而 fail closed，
# 遠端 bot 就一顆子 pane／子 agent 都開不出來。遠端子 pane 本來也登記不到 daemon（/relay/pane 一樣要 AM_PORT），
# fence 在那邊保護不到任何東西。
am_spawn_fenced() {
    [ -n "${AM_BOT_ID:-}" ] && [ -n "${AM_PORT:-}" ]
}

am_spawn_begin() {
    _AM_SPAWN_PERMIT=""
    am_spawn_fenced || return 0
    _tok=$(am_bot_token)
    if [ -z "$_tok" ] || ! command -v curl >/dev/null 2>&1; then
        printf 'agents-manager: 無法確認憑證輪替狀態，拒絕建立會繼承 bot credential 的 pane\n' >&2
        return 75
    fi
    _out=$(curl -s -m 2 -w '\n%{http_code}' -X POST "http://127.0.0.1:${AM_PORT}/relay/spawn/begin" \
        -H "X-AM-Bot-Token: $_tok" \
        --data-urlencode "bot_id=${AM_BOT_ID}" \
        --data-urlencode "timeout_ms=${_AM_SPAWN_TIMEOUT_MS:-}" 2>/dev/null)
    _rc=$?
    _code=$(printf '%s\n' "$_out" | tail -n 1)
    _body=$(printf '%s\n' "$_out" | sed '$d')
    if [ "$_rc" -ne 0 ] || [ "$_code" != 200 ]; then
        printf 'agents-manager: credential rotation 或 daemon 暫時不可確認，沒有建立子 pane；請稍後重試\n' >&2
        return 75
    fi
    _AM_SPAWN_PERMIT=$(printf '%s\n' "$_body" | sed -n 's/.*"permit_id" *: *"\([^"]*\)".*/\1/p' | head -n 1)
    if [ -z "$_AM_SPAWN_PERMIT" ]; then
        printf 'agents-manager: daemon 沒有回 spawn permit，拒絕建立子 pane\n' >&2
        return 75
    fi
    return 0
}

am_spawn_finish() {
    _pane=$1
    _purpose=$2
    am_spawn_fenced || return 0
    [ -n "${_AM_SPAWN_PERMIT:-}" ] || return 75
    if [ -z "$_pane" ]; then
        printf 'agents-manager: herdr 沒回新 pane id；保留 spawn fence，拒絕假裝已登記\n' >&2
        return 75
    fi
    _tok=$(am_bot_token)
    if [ -z "$_tok" ] || [ -z "${AM_PORT:-}" ] || ! command -v curl >/dev/null 2>&1; then
        printf 'agents-manager: 無法登記憑證繼承 pane；保留 spawn fence\n' >&2
        return 75
    fi
    _out=$(curl -s -m 2 -w '\n%{http_code}' -X POST "http://127.0.0.1:${AM_PORT}/relay/spawn/finish" \
        -H "X-AM-Bot-Token: $_tok" \
        --data-urlencode "bot_id=${AM_BOT_ID}" \
        --data-urlencode "permit_id=${_AM_SPAWN_PERMIT}" \
        --data-urlencode "pane_id=${_pane}" \
        --data-urlencode "purpose=${_purpose}" 2>/dev/null)
    _rc=$?
    _code=$(printf '%s\n' "$_out" | tail -n 1)
    if [ "$_rc" -ne 0 ] || [ "$_code" != 200 ]; then
        printf 'agents-manager: 新 pane 尚未登記，保留 spawn fence 以免憑證失效；請回報 daemon 狀態\n' >&2
        return 75
    fi
    return 0
}

# herdr 沒開出 pane（或 shim 被中斷）時丟掉 permit。沒有 permit 就什麼都不做。
am_spawn_abort() {
    [ -n "${_AM_SPAWN_PERMIT:-}" ] || return 0
    [ -n "${AM_BOT_ID:-}" ] || return 0
    _tok=$(am_bot_token)
    if [ -n "$_tok" ] && [ -n "${AM_PORT:-}" ] && command -v curl >/dev/null 2>&1; then
        curl -s -m 2 -X POST "http://127.0.0.1:${AM_PORT}/relay/spawn/abort" \
            -H "X-AM-Bot-Token: $_tok" \
            --data-urlencode "bot_id=${AM_BOT_ID}" \
            --data-urlencode "permit_id=${_AM_SPAWN_PERMIT}" >/dev/null 2>&1 || true
    fi
    _AM_SPAWN_PERMIT=""
    return 0
}

# `<name>` → `<AM_AGENT_NAME>-<name>`, unless it already carries the prefix. herdr agent names
# are `[a-z][a-z0-9_-]{0,31}` (32 chars). A blind `cut -c1-32` turns a 32-char parent into itself
# and collapses every child of a 28–31 char parent onto one prefix (#665). When the readable
# name does not fit, shorten the parent and append a 6-digit hash of the requested suffix.
# Never return the parent name.
am_child_name() {
    _n=$1
    if [ -z "${AM_AGENT_NAME:-}" ]; then
        printf '%s' "$_n"
        return 0
    fi
    case "$_n" in
        "$AM_AGENT_NAME"-*)
            printf '%s' "$_n"
            return 0
            ;;
    esac
    _full=$(printf '%s-%s' "$AM_AGENT_NAME" "$_n")
    if [ "${#_full}" -gt 32 ]; then
        _sum=$(printf '%s' "$_n" | cksum 2>/dev/null | awk 'NR==1 { print $1 }')
        if [ -z "$_sum" ]; then
            printf 'agents-manager: 子 agent 名稱放不進 32 字，而且算不出尾碼，沒有改成母 bot `%s`\n' "$AM_AGENT_NAME" >&2
            return 75
        fi
        _tail=$(printf '%06d' "$((_sum % 1000000))")
        _pre=$(printf '%s' "$AM_AGENT_NAME" | cut -c1-25 | sed 's/-*$//')
        [ -n "$_pre" ] || _pre=p
        _full=$(printf '%s-%s' "$_pre" "$_tail")
    fi
    if [ "$_full" = "$AM_AGENT_NAME" ] || [ "${#_full}" -gt 32 ]; then
        printf 'agents-manager: 子 agent 名稱放不進 32 字（母 agent 是 `%s`），沒有改成母 bot 自己\n' "$AM_AGENT_NAME" >&2
        return 75
    fi
    case "$_full" in
        "$AM_AGENT_NAME"-*) ;;
        *)
            # 母名太長、尾碼路徑把前綴截短了：結果必須仍比「空尾碼」長，且不能是母名本身（上面已擋）。
            case "$_full" in
                *-*) ;;
                *)
                    printf 'agents-manager: 子 agent 名稱放不進 32 字（母 agent 是 `%s`），沒有改成母 bot 自己\n' "$AM_AGENT_NAME" >&2
                    return 75
                    ;;
            esac
            ;;
    esac
    printf 'agents-manager: 子 agent 已改名為 `%s`，才會掛在 `%s` 底下被追蹤\n' "$_full" "$AM_AGENT_NAME" >&2
    printf '%s' "$_full"
}

# `herdr agent start [flags] <name> …` — rewrite the first bare word after `start`, which is
# the agent name. Flags may come first, so skip options and the values of the ones that take
# one, and stop at `--` (everything after it is the agent's own argv).
am_agent_start() {
    shift 2
    # `--help` 只是查用法：不開 pane、不補 env。以前照樣走 spawn 流程，env 補送就打進呼叫者自己的 pane（目標預設是 $HERDR_PANE_ID）。
    for _a in "$@"; do
        case "$_a" in
            --) break ;;
            -h | --help) exec "$AM_HERDR" agent start "$@" ;;
        esac
    done
    if [ -n "${AM_CHILD_OF:-}" ]; then
        printf 'agents-manager: 你是 `%s` 派出的子 agent，禁止再開子 agent。需要更多人手：停下來在回報裡寫清楚要另派什麼、為什麼，由 `%s` 決定要不要開兄弟 agent\n' "$AM_CHILD_OF" "$AM_CHILD_OF" >&2
        exit 77
    fi
    _n=$#
    _i=0
    _named=0
    _stop=0
    _prev=""
    _kind=""
    _pane=""
    _has_model=0
    _has_effort=0
    _has_instr=0
    _has_docs=0
    _has_profile=0
    while [ "$_i" -lt "$_n" ]; do
        _a=$1
        shift
        _i=$((_i + 1))
        _orig=$_a
        # 保留變數（見 am_forward_with_env）：`--` 之前帶這兩個 key 的 `--env` 一律丟掉。
        # 只剝、不補：`herdr agent start` 沒有 `--env`（它在既有 pane 裡開 agent，env 在建 pane 時就注入了），
        # 補上去會變成未知旗標，每一次開 child 都失敗（sol 六輪，herdr 0.8.2 實測）。
        if [ "$_stop" = 0 ]; then
            case "$_a" in
                --env)
                    if [ "$_i" -lt "$_n" ]; then
                        case "$1" in
                            AM_INSTANCE=* | AM_DATA_DIR=* | AM_CHILD_OF=*)
                                shift
                                _i=$((_i + 1))
                                continue
                                ;;
                        esac
                    fi
                    ;;
                --env=AM_INSTANCE=* | --env=AM_DATA_DIR=* | --env=AM_CHILD_OF=*) continue ;;
            esac
        fi
        # 只看 `--` 之前（之後是 agent 自己的 argv）；clap 的 `--kind=X`／`--pane=X` 與空白拼法同義。
        if [ "$_stop" = 0 ]; then
            if [ "$_prev" = "--kind" ]; then _kind=$_a; fi
            if [ "$_prev" = "--pane" ]; then _pane=$_a; fi
            if [ "$_prev" = "--timeout" ]; then _AM_SPAWN_TIMEOUT_MS=$_a; fi
            case "$_a" in
                --kind=*) _kind=${_a#--kind=} ;;
                --pane=*) _pane=${_a#--pane=} ;;
                --timeout=*) _AM_SPAWN_TIMEOUT_MS=${_a#--timeout=} ;;
            esac
        fi
        if [ "$_stop" = 1 ]; then
            # codex / grok spell it `-m`, codex also `-c model=…`: all of them are "the child
            # picked its own model" and must not be overridden with the parent's.
            case "$_a" in
                --model | --model=* | -m | -m=* | model=*) _has_model=1 ;;
                --effort | --effort=*) _has_effort=1 ;;
                -c) : ;;
                model_reasoning_effort=*) _has_effort=1 ;;
                --append-system-prompt | --append-system-prompt=* | --append-system-prompt-file* | developer_instructions=* | --rules | --rules=*) _has_instr=1 ;;
                project_doc_max_bytes=*) _has_docs=1 ;;
                -p | --profile | --profile=*) _has_profile=1 ;;
            esac
        elif [ "$_named" = 0 ]; then
            case "$_prev" in
                --kind | --pane | --timeout) : ;;
                *)
                    case "$_a" in
                        --) _stop=1 ;;
                        -*) : ;;
                        *)
                            _a=$(am_child_name "$_a") || exit $?
                            _named=1
                            ;;
                    esac
                    ;;
            esac
        else
            case "$_a" in
                --) _stop=1 ;;
            esac
        fi
        _prev=$_orig
        set -- "$@" "$_a"
    done
    # 沒指定模型的子 agent 會跑 CLI 的預設（claude 現在是 fable），跟母 bot 明明選的 opus 對不上，
    # 側欄就多出一顆「claude-fable-5-1」看不懂的。母 bot 的模型／強度在 AM_MODEL / AM_EFFORT，
    # 同 kind 就補上；自己有寫 --model 的一律尊重。
    if [ -n "${AM_MODEL:-}" ] && [ "$_has_model" = 0 ] && { [ -z "$_kind" ] || [ "$_kind" = "${AM_KIND:-}" ]; }; then
        [ "$_stop" = 1 ] || set -- "$@" --
        _stop=1
        set -- "$@" --model "$AM_MODEL"
        if [ -n "${AM_EFFORT:-}" ] && [ "$_has_effort" = 0 ] && [ "${AM_KIND:-}" = "claude" ]; then
            set -- "$@" --effort "$AM_EFFORT"
        fi
        printf 'agents-manager: 子 agent 沒指定模型，沿用母 bot 的 `%s`\n' "$AM_MODEL" >&2
    fi
    # §6.5i：子 agent 的指示跟母 bot 同一個來源（`AM_INSTRUCTIONS_FILE`＝AG Man 規則＋`[agents]` 的 agent md），
    # CLI 自己的指示檔一律不讀：claude 靠繼承的 CLAUDE_CODE_DISABLE_CLAUDE_MDS，codex 補 project_doc_max_bytes=0。
    # 呼叫者自己帶了就尊重。注入的一律是單行短參數（路徑／profile 名）：herdr ≥0.9.0 擋含換行、tab 等控制字元的參數（#772）。
    _instr=""
    if [ "$_has_instr" = 0 ] && [ -n "${AM_INSTRUCTIONS_FILE:-}" ] && [ -r "$AM_INSTRUCTIONS_FILE" ]; then
        _instr=$AM_INSTRUCTIONS_FILE
    fi
    case "${_kind:-${AM_KIND:-}}" in
        claude)
            if [ -n "$_instr" ]; then
                [ "$_stop" = 1 ] || set -- "$@" --
                _stop=1
                set -- "$@" --append-system-prompt-file "$_instr"
            fi
            ;;
        codex)
            if [ -n "$_instr" ]; then
                [ "$_stop" = 1 ] || set -- "$@" --
                _stop=1
                if [ "$_has_profile" = 1 ]; then
                    printf 'agents-manager: 你自己帶了 codex 的 -p/--profile，子 agent 這次沒帶指示檔 %s\n' "$_instr" >&2
                elif _prof=$(am_codex_instructions_profile "$_instr"); then
                    set -- "$@" -p "$_prof"
                elif grep -qF "'''" "$_instr" 2>/dev/null; then
                    printf 'agents-manager: %s 含有三個連續單引號，TOML 表示不了，子 agent 這次沒帶指示檔\n' "$_instr" >&2
                else
                    printf 'agents-manager: 寫不進 codex 的 profile（%s），子 agent 這次沒帶指示檔\n' "${CODEX_HOME:-$HOME/.codex}" >&2
                fi
            fi
            # 只在 §6.5i 底下（daemon 給了 AM_INSTRUCTIONS_FILE）才關：人工 shell 的 codex 照它自己的習慣。
            if [ "$_has_docs" = 0 ] && [ -n "${AM_INSTRUCTIONS_FILE:-}" ]; then
                [ "$_stop" = 1 ] || set -- "$@" --
                _stop=1
                set -- "$@" -c project_doc_max_bytes=0
            fi
            ;;
        grok)
            if [ -n "$_instr" ]; then
                [ "$_stop" = 1 ] || set -- "$@" --
                _stop=1
                # grok 的 `--rules` 只收字串、沒有讀檔版：給一行指向檔案的指示，讓它開工前自己讀。
                set -- "$@" --rules "AG Man 指示（硬規則，效力同系統指示）在 $_instr：開始任何工作前先完整讀過並照做。"
            fi
            ;;
    esac
    # 目標 pane 只認明寫的 `--pane`，**不退回 $HERDR_PANE_ID**：那是呼叫者自己的 pane（裡面跑著它自己的 agent），
    # 補送 env 的那一行會被打進它的輸入框——2026-10-01 `pane split` 失敗、`--pane ""` 傳下來時就這樣灌了四行給使用者。
    # 指到自己的 pane 也一樣擋：不能在自己正在跑的 agent 裡再開 agent。
    if [ -n "$_pane" ] && [ "$_pane" = "${HERDR_PANE_ID:-}" ]; then
        printf 'agents-manager: agent start 的 --pane 是你自己的 pane（%s）；先 herdr pane split 開新 pane，再把新 pane id 給 --pane\n' "$_pane" >&2
        exit 2
    fi
    if [ -n "${AM_BOT_ID:-}" ] && [ -z "$_pane" ]; then
        printf 'agents-manager: agent start 沒有 --pane（或是空的，多半是前面的 pane split 失敗）；先 herdr pane split 開新 pane，再把新 pane id 給 --pane。沒有呼叫 herdr\n' >&2
        exit 2
    fi
    am_spawn_begin || exit $?
    trap 'am_spawn_abort; exit 130' INT TERM
    am_reexport_env_before_start "$_pane"
    if [ -z "${AM_BOT_ID:-}" ]; then
        exec "$AM_HERDR" agent start "$@"
    fi
    _out=$("$AM_HERDR" agent start "$@")
    _rc=$?
    [ -z "$_out" ] || printf '%s\n' "$_out"
    if [ "$_rc" -ne 0 ]; then
        am_spawn_abort
        exit "$_rc"
    fi
    trap - INT TERM
    am_spawn_finish "$_pane" "" || exit $?
    exit 0
}

# issue #57：`agent start` 沒有 `--env`，全靠假設「目標 pane 是 pane split 剛開的、帳號早就注入了」——
# 漏了那一步、或重用一顆沒走過那條路的舊 pane 時，子 agent 就默默吃到預設帳號（cc0）的額度。
# 補送一行 export 到 `--pane` 指到的目標，跟 agent.start 前補 PATH（`start_inner`）用同一招：
# pty 會緩衝這行輸入，pane 還沒起殻也不怕；pane 早有正確值時只是重覆設一次，無害。
am_reexport_env_before_start() {
    _pane=$1
    [ -n "$_pane" ] || return 0
    _body=""
    _line=""
    for _k in $AM_RESERVED_ENV_KEYS AM_INSTANCE AM_DATA_DIR; do
        eval "_v=\${$_k:-}"
        [ -n "$_v" ] || continue
        [ "$_k" != PATH ] || _v=$(am_dedupe_path "$_v")
        _esc=$(printf '%s' "$_v" | sed "s/'/'\\\\''/g")
        _body="${_body}export $_k='$_esc'
"
        _line="${_line}export $_k='$_esc'; "
    done
    _v=$(am_child_of_value)
    if [ -n "$_v" ]; then
        _esc=$(printf '%s' "$_v" | sed "s/'/'\\\\''/g")
        _body="${_body}export AM_CHILD_OF='$_esc'
"
        _line="${_line}export AM_CHILD_OF='$_esc'; "
    fi
    [ -n "$_body" ] || return 0
    # #389：整串 export 一行行打進 pane 會灌滿終端畫面（沒 hook 的 bot 靠快照補回覆也會讀到）。
    # 寫進 0600 暫存檔，pane 只收一行 ` . '<檔>' && rm -f '<檔>'`（行首空白：不進 shell history）。
    # 暫存檔放 $TMPDIR（不能是 scratchpad／outbox）或 /tmp；寫不出來才退回舊的逐行 export，環境不能丟。
    _dir=${TMPDIR:-/tmp}
    case "$_dir" in
        *scratchpad* | */outbox/* | */outbox) _dir=/tmp ;;
    esac
    _f=$(umask 077; mktemp "${_dir%/}/am-env.XXXXXX" 2>/dev/null) || _f=""
    if [ -n "$_f" ] && printf '%s' "$_body" > "$_f" 2>/dev/null; then
        chmod 600 "$_f" 2>/dev/null
        _qf=$(printf '%s' "$_f" | sed "s/'/'\\\\''/g")
        "$AM_HERDR" pane send-text "$_pane" " . '$_qf' && rm -f '$_qf'
" >/dev/null 2>&1
        return 0
    fi
    [ -z "$_f" ] || rm -f "$_f"
    "$AM_HERDR" pane send-text "$_pane" "$_line
" >/dev/null 2>&1
}

# 寫給 daemon 問過、但**不能**直送的結果（issue #143）：講明原因、不碰 pane，明確失敗讓寄件端重試。
am_announce_refuse() {
    printf 'agents-manager: 寫給 %s 的訊息沒有送出（%s）；不直接打進它的 pane——直送會繞過協調佇列，變成佇列一份、pane 一份。請稍後重試\n' "$2" "$3" >&2
    exit "$1"
}

# 遠端 bot（有 bot 身分、沒有 `AM_PORT`：遠端不開回 daemon 的埠）打不到 `/relay/announce`，收件方的回音就會被當成
# 使用者打的字（2026-09-30 使用者）。改寫一則報備進自己 bot 目錄的 hook spool，daemon 收 hook 時一起收。
# 寫不了（沒有目錄、沒有 python3）就算了，照舊直送：標不出來源不是送不出去的理由。
am_spool_relay() {
    _sd="$HOME/.config/agents-manager${AM_INSTANCE:+/instances/$AM_INSTANCE}/bots/${AM_BOT_ID}"
    [ -d "$_sd" ] || return 0
    command -v python3 >/dev/null 2>&1 || return 0
    python3 - "$_sd/hook-spool.d" "$AM_BOT_ID" "${AM_KIND:-claude}" "$1" "$2" <<'AMPY' >/dev/null 2>&1 || true
import datetime, json, os, sys, tempfile, time
sd, bot, kind, to_agent, text = sys.argv[1:6]
os.makedirs(sd, exist_ok=True)
# 跟 daemon 的 db::now() 同一種毫秒 UTC 格式：hook_events.received_at 只存這種（issue #101／#730）。
now = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")
body = {"bot_id": bot, "provider": kind, "truncated": False, "run_id": "",
        "received_at": now,
        "payload": {"hook_event_name": "AmRelayAnnounce", "to_agent": to_agent, "text": text}}
fd, tmp = tempfile.mkstemp(prefix=".tmp.", dir=sd)
with os.fdopen(fd, "w") as f:
    f.write(json.dumps(body, ensure_ascii=False) + "\n")
os.replace(tmp, os.path.join(sd, "%d-%d.json" % (time.time_ns(), os.getpid())))
AMPY
}

am_agent_prompt() {
    shift 2
    case "${1:-}" in
        -* | "")
            # 旗標在名字前面（或根本沒給名字）：交給真的 herdr 去講清楚，我們不猜。
            exec "$AM_HERDR" agent prompt "$@"
            ;;
    esac
    _name=$1
    shift
    # 原名本來就存在（AGM、其他頂層 bot、pane id）就照原名送；硬補前綴只會變成 unknown_target，
    # 訊息沒送到、stderr 還說「已改名」。找不到才當成自己的子 agent 補前綴。
    if ! "$AM_HERDR" agent get "$_name" >/dev/null 2>&1; then
        _name=$(am_child_name "$_name") || exit $?
    fi
    # 我們自己的旗標（SPEC §18.15）：`--ack`（純告知，不叫醒 AGM）、`--reply-to <id>`（回哪一則事件／交辦）。
    # 真的 herdr 不認得，一律剝掉；沒帶就是新的事，AGM 會被叫醒。
    _ack=""
    _reply_to=""
    _n=$#
    _i=0
    while [ "$_i" -lt "$_n" ]; do
        _a=$1
        shift
        _i=$((_i + 1))
        case "$_a" in
            --ack)
                _ack=1
                continue
                ;;
            --reply-to)
                if [ "$_i" -lt "$_n" ]; then
                    _reply_to=$1
                    shift
                    _i=$((_i + 1))
                fi
                continue
                ;;
            --reply-to=*)
                _reply_to=${_a#--reply-to=}
                continue
                ;;
        esac
        set -- "$@" "$_a"
    done
    # 正文只是 TEXT 那個位置參數：`--wait`、`--until X`、`--timeout X` 是 herdr 的旗標，混進正文會讓
    # 同一句申請因為逾時值不同就變成兩個指紋。
    _text=""
    _skip=0
    for _a in "$@"; do
        if [ "$_skip" = 1 ]; then
            _skip=0
            continue
        fi
        case "$_a" in
            --wait | --until=* | --timeout=*) continue ;;
            --until | --timeout)
                _skip=1
                continue
                ;;
        esac
        _text="${_text:+$_text }$_a"
    done
    if [ -n "${AM_BOT_ID:-}" ] && [ -n "$(am_bot_token)" ] && [ -n "${AM_PORT:-}" ]; then
        # 「確定沒送到 daemon」與「送了但回覆不明」要分清楚（issue #143）。直送 pane 只有一種情形站得住腳：**連線根本沒建立**
        # （curl 7，daemon 不在），request 一定沒進 daemon。其餘——逾時、空回應、連線被重置、任何非 2xx、看不懂的 2xx——
        # request 可能已經進了 daemon（durable 的 bot_request 可能已經 commit，只是回覆沒收完整）：直送會變成佇列一份、pane 一份，
        # 而且 401／403 這種身分被拒也不能變成繞過控制面的旁路。這些一律明確失敗（exit 75／77），讓寄件端自己重試。
        if ! command -v curl >/dev/null 2>&1; then
            am_announce_refuse 75 "$_name" "這台機器沒有 curl，問不了 daemon 這句要不要走協調佇列"
        fi
        _try=0
        while :; do
            _try=$((_try + 1))
            # 第一次 2 秒；再問就給久一點（daemon 可能已經排進佇列、只是回得慢）。沒帶 request id 的申請 daemon 以內容指紋去重，重問不會變兩筆。
            if [ "$_try" = 1 ]; then _m=2; else _m=15; fi
            # 表單編碼：prompt 內容有引號、換行、`&` 都不會壞，也不必在 sh 裡拼 JSON。`-w` 在回應後面補一行 HTTP 狀態碼。
            _out=$(curl -s -m "$_m" -w '\n%{http_code}' -X POST "http://127.0.0.1:${AM_PORT}/relay/announce" \
                -H "X-AM-Bot-Token: $(am_bot_token)" \
                --data-urlencode "bot_id=${AM_BOT_ID}" \
                --data-urlencode "to_agent=${_name}" \
                --data-urlencode "text=${_text}" \
                --data-urlencode "ack=${_ack}" \
                --data-urlencode "reply_to=${_reply_to}" 2>/dev/null)
            _rc=$?
            case "$_rc" in 0 | 7) break ;; esac
            [ "$_try" -lt 2 ] || break
        done
        if [ "$_rc" != 7 ]; then
            if [ "$_rc" != 0 ]; then
                am_announce_refuse 75 "$_name" "問 daemon 時連線出了狀況（curl 結束碼 ${_rc}），不確定它有沒有收到；同一句再送一次就好，daemon 以內容去重"
            fi
            _code=$(printf '%s\n' "$_out" | tail -n 1)
            _resp=$(printf '%s\n' "$_out" | sed '$d')
            case "$_resp" in
                # 寫給 AGM、但 daemon 現在排不進協調佇列（issue #143）：不直送，明確失敗讓寄件端重試。
                # 直送會繞過 durable inbox 與去重，控制面也不知道這則走了旁路。
                *'"routing_unavailable":'*)
                    printf 'agents-manager: 寫給 %s 的訊息現在排不進 AGM 協調佇列，沒有送出、也不直接打進它的 pane；請稍後重試：%s\n' "$_name" "$_resp" >&2
                    exit 75
                    ;;
            esac
            case "$_code" in
                2??)
                    case "$_resp" in
                        # 寫給 AGM 的申請 daemon 已經排進協調者的佇列（SPEC §18.15）：不再打進 AGM 的 pane，否則同一句話會先燒一輪巡檢的回合。
                        *'"routed":'*)
                            printf 'agents-manager: 已排入 AGM 協調佇列，不直接打進 %s 的 pane：%s\n' "$_name" "$_resp" >&2
                            exit 0
                            ;;
                        # daemon 明確說「不是給協調者的」（announce 已記下）：照舊直送。
                        '{}') : ;;
                        *) am_announce_refuse 75 "$_name" "daemon 回了看不懂的內容：$(printf '%s' "$_resp" | cut -c1-200)" ;;
                    esac
                    ;;
                401 | 403)
                    am_announce_refuse 77 "$_name" "daemon 不認這顆 bot 的身分（HTTP ${_code}）：$(printf '%s' "$_resp" | cut -c1-200)"
                    ;;
                *)
                    am_announce_refuse 75 "$_name" "daemon 回 HTTP ${_code}：$(printf '%s' "$_resp" | cut -c1-200)"
                    ;;
            esac
        fi
    elif [ -n "${AM_BOT_ID:-}" ] && [ -z "${AM_PORT:-}" ]; then
        am_spool_relay "$_name" "$_text"
    fi
    exec "$AM_HERDR" agent prompt "$_name" "$@"
}

# herdr spawns a pane from the *server*, not from this shell, so nothing is inherited: a child
# pane would come up on the user's default account, with no hook token and no way to name its
# own children. Pass the parent's environment down explicitly, without overriding a value the
# caller set by hand.
#
# 例外是 AM_INSTANCE、AM_DATA_DIR：它們決定 child 的 hook／spool 屬於哪顆 daemon，是保留變數。
# 呼叫者自帶的 `--env KEY=…`／`--env=KEY=…` 一律剝掉，再照母 pane 的實際值補（母 pane 沒有就不帶）；
# 否則 child 可以偽造隔離 slug 或清掉它，把 grok hook／spool 送進別的實例（sol 五輪）。
am_forward_with_env() {
    _sub1=$1
    _sub2=$2
    shift 2
    _purpose=""
    _has_workspace=0
    _n=$#
    _i=0
    while [ "$_i" -lt "$_n" ]; do
        _a=$1
        shift
        _i=$((_i + 1))
        case "$_a" in
            # 我們自己的旗標（§6.5e 的用途標記），不轉給 herdr。
            --purpose)
                if [ "$_i" -lt "$_n" ]; then
                    _purpose=$1
                    shift
                    _i=$((_i + 1))
                fi
                continue
                ;;
            --purpose=*)
                _purpose=${_a#--purpose=}
                continue
                ;;
            --workspace | --workspace=*) _has_workspace=1 ;;
            --env)
                if [ "$_i" -lt "$_n" ]; then
                    case "$1" in
                        AM_INSTANCE=* | AM_DATA_DIR=* | AM_CHILD_OF=*)
                            shift
                            _i=$((_i + 1))
                            continue
                            ;;
                    esac
                fi
                ;;
            --env=AM_INSTANCE=* | --env=AM_DATA_DIR=* | --env=AM_CHILD_OF=*) continue ;;
        esac
        set -- "$@" "$_a"
    done
    for _k in AM_INSTANCE AM_DATA_DIR; do
        eval "_v=\${$_k:-}"
        [ -z "$_v" ] || set -- "$@" --env "$_k=$_v"
    done
    _v=$(am_child_of_value)
    [ -z "$_v" ] || set -- "$@" --env "AM_CHILD_OF=$_v"
    # 呼叫者自己設過的 key 不補母 pane 的值。比對要**兩種拼法都認**（`--env K=V` 與 `--env=K=V`），
    # 而且只看 --env 的位置：以前用 `case " $* "` 比整串 argv，`--env=K=V` 因為前面是 `=` 不算數，
    # 於是同一個 key 被補第二份（子 agent 可能跑在母 bot 的帳號下），而隨便一個參數的值裡含有
    # " AM_PORT=" 之類的字樣又會讓那個 env 整個不被傳下去（review 2026-09-16）。
    _seen=""
    _n=$#
    _i=0
    while [ "$_i" -lt "$_n" ]; do
        _a=$1
        shift
        _i=$((_i + 1))
        case "$_a" in
            --env)
                [ "$_i" -lt "$_n" ] && _seen="$_seen ${1%%=*}"
                ;;
            --env=*)
                _rest=${_a#--env=}
                _seen="$_seen ${_rest%%=*}"
                ;;
        esac
        set -- "$@" "$_a"
    done
    # §6.5e：`tab create` 沒指定 workspace 時落在**專案自己的** workspace，不要開到別的專案去
    # （w168 收到 wt 的 dev server 就是這樣來的）。`pane split` 以母 pane 為基準，本來就同 workspace。
    # 先問 herdr 母 pane（$HERDR_PANE_ID）**現在**在哪個 workspace，問不到才用 AM_WORKSPACE_ID：daemon 在決定 workspace
    # **之前**就把它算進 env，第一次啟動或 herdr 重開後根本沒有，舊映射失效改開新 workspace 時還是死掉的 id（review core 7）。
    if [ "$_has_workspace" = 0 ] && [ "$_sub1 $_sub2" = "tab create" ]; then
        _ws=""
        if [ -n "${HERDR_PANE_ID:-}" ]; then
            _ws=$("$AM_HERDR" pane get "$HERDR_PANE_ID" 2>/dev/null | tr ',' '\n' | sed -n 's/.*"workspace_id" *: *"\([^"]*\)".*/\1/p' | head -n 1)
        fi
        [ -n "$_ws" ] || _ws=${AM_WORKSPACE_ID:-}
        if [ -n "$_ws" ]; then
            set -- "$@" --workspace "$_ws"
            # 子 pane 繼承的也是實際落點，不是母 bot 那份可能過期的值。
            AM_WORKSPACE_ID=$_ws
        fi
    fi
    for _k in $AM_RESERVED_ENV_KEYS; do
        eval "_v=\${$_k:-}"
        [ -n "$_v" ] || continue
        [ "$_k" != PATH ] || _v=$(am_dedupe_path "$_v")
        case " $_seen " in
            *" $_k "*) continue ;;
        esac
        set -- "$@" --env "$_k=$_v"
    done
    # Every managed pane creation is fenced, even without a display purpose: an empty pane still
    # retains AM_BOT_ID / AM_BOT_TOKEN and can start a child after the parent restarts.
    if am_spawn_fenced; then
        am_spawn_begin || exit $?
        trap 'am_spawn_abort; exit 130' INT TERM
        _out=$("$AM_HERDR" "$_sub1" "$_sub2" "$@")
        _rc=$?
        [ -z "$_out" ] || printf '%s\n' "$_out"
        _pane=$(printf '%s' "$_out" | tr ',' '\n' | sed -n 's/.*"pane_id" *: *"\([^"]*\)".*/\1/p' | head -n 1)
        if [ "$_rc" -ne 0 ]; then
            if [ -z "$_pane" ]; then
                am_spawn_abort
            else
                am_spawn_finish "$_pane" "$_purpose" || exit $?
            fi
            exit "$_rc"
        fi
        trap - INT TERM
        am_spawn_finish "$_pane" "$_purpose" || exit $?
        exit 0
    fi
    # §6.5e：開出來的 pane 要能說出「這是誰、為了什麼開的」。歸屬由 daemon 從行程環境推斷（AM_BOT_ID
    # 一定帶得下去），這裡只補**用途**：`--purpose <文字>` 是我們自己的旗標，轉發前剝掉。
    # 沒有 curl／沒有 token 就只是少一個字串，pane 照開。
    if [ -n "$_purpose" ] && [ -n "${AM_BOT_ID:-}" ] && [ -n "$(am_bot_token)" ] && [ -n "${AM_PORT:-}" ] && command -v curl >/dev/null 2>&1; then
        _out=$("$AM_HERDR" "$_sub1" "$_sub2" "$@")
        _rc=$?
        printf '%s\n' "$_out"
        [ "$_rc" -eq 0 ] || exit "$_rc"
        # 回應裡第一個 pane_id 就是剛開出來的那顆（herdr 0.8.2 schema：pane_created 只有 `pane`，tab_created／
        # workspace_created 只有 `root_pane` 帶 pane_id）。冒號兩邊有沒有空白都認。
        _pane=$(printf '%s' "$_out" | tr ',' '\n' | sed -n 's/.*"pane_id" *: *"\([^"]*\)".*/\1/p' | head -n 1)
        [ -n "$_pane" ] || exit 0
        curl -s -m 2 -X POST "http://127.0.0.1:${AM_PORT}/relay/pane" \
            -H "X-AM-Bot-Token: $(am_bot_token)" \
            --data-urlencode "bot_id=${AM_BOT_ID}" \
            --data-urlencode "pane_id=${_pane}" \
            --data-urlencode "purpose=${_purpose}" >/dev/null 2>&1 || true
        exit 0
    fi
    exec "$AM_HERDR" "$_sub1" "$_sub2" "$@"
}

AM_HERDR=$(am_real_herdr | head -n 1)
if [ -z "$AM_HERDR" ]; then
    printf 'agents-manager: 找不到真正的 herdr（把它的路徑放進 AM_REAL_HERDR）\n' >&2
    exit 127
fi

case "${1:-} ${2:-}" in
    "agent start") am_agent_start "$@" ;;
    "agent prompt") am_agent_prompt "$@" ;;
    # `workspace create` 也會開一個 root pane（herdr 0.8.2 有 `--env`）。`worktree create/open` 同樣開 workspace，
    # 但沒有 `--env` 可帶：那個 pane 由 herdr server 開，什麼 AM_* 都拿不到，hook 不會觸發，也就不會送錯實例。
    "pane split" | "pane new" | "tab create" | "workspace create") am_forward_with_env "$@" ;;
    *) exec "$AM_HERDR" "$@" ;;
esac
"##;

/// Rewritten every start so an upgraded daemon never leaves an old shim behind.
pub fn install_local(bot_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = crate::shim_refresh::bin_dir(bot_dir);
    std::fs::create_dir_all(&dir)?;
    // 暫存檔 + rename：直接覆寫的話，正在跑的那支 shim 會讀到寫到一半的內容，而且中途死掉會留下
    // 一個不能執行的檔案（`shim_refresh::write_atomic`）。內容一樣就不動。
    crate::shim_refresh::write_atomic(&dir.join("herdr"), SHIM_SH)?;
    Ok(dir)
}

/// Same ssh path as `hook.sh` (SPEC §11.4). 暫存檔＋chmod＋rename（原子），內容一樣就不重寫——
/// 跟本機、跟重連後的補版（`shim_refresh::sync_remote`）是同一支腳本（issue #124）。
pub async fn install_remote(conn: &crate::hosts::HostConn, remote_bot_dir: &str) -> anyhow::Result<String> {
    let r = crate::shim_refresh::sync_remote(conn, &[remote_bot_dir.to_string()], &["herdr"], true).await?;
    if !r.failed.is_empty() {
        anyhow::bail!("remote herdr shim install failed: {:?}", r.failed);
    }
    Ok(format!("{remote_bot_dir}/bin"))
}

#[cfg(test)]
mod tests {
    //! Runs the real script against a fake `herdr` that prints argv one per line.
    use std::path::Path;
    use std::process::Command;

    /// 剛寫好的腳本立刻 exec 可能撞上 `ETXTBSY`（Text file busy）（issue #189）：並行的另一條測試在別的執行緒 `fork`，
    /// 短暫繼承了這個檔案的寫入 fd，直到它自己 `exec` 為止；這邊在那一刻 exec 就回 `Text file busy`。這不是被測程式的問題，是測試
    /// 基礎設施的競態。重試的條件是「exec 還在被擋」，不是睡一個固定的時間：等到 exec **真的成功**才往下。
    fn is_text_file_busy(e: &std::io::Error) -> bool {
        e.raw_os_error() == Some(libc::ETXTBSY)
    }

    fn output_retrying(cmd: &mut Command) -> std::process::Output {
        let started = std::time::Instant::now();
        loop {
            match cmd.output() {
                Err(e) if is_text_file_busy(&e) && started.elapsed() < std::time::Duration::from_secs(30) => std::thread::sleep(std::time::Duration::from_millis(2)),
                r => return r.unwrap(),
            }
        }
    }

    /// 寫一支測試用腳本（`body` 接在 `#!/bin/sh` 之後）並確定它**已經可以被 exec**：腳本第一行在 `AM_TEST_EXEC_PROBE` 有設時直接 `exit 0`，
    /// 寫完後用它 exec 一次、`ETXTBSY` 就重試。exec 成功那一刻沒有任何行程握著它的寫入 fd，之後（含被測的 shim 自己去 exec 這支腳本）
    /// 就不會再撞上——只有我們會開它來寫，而我們已經寫完了。
    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n[ -z \"${{AM_TEST_EXEC_PROBE:-}}\" ] || exit 0\n{body}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = output_retrying(Command::new(path).env("AM_TEST_EXEC_PROBE", "1"));
        assert!(out.status.success(), "{}: {:?}", path.display(), out.status);
    }

    struct Sandbox {
        dir: std::path::PathBuf,
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Sandbox {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("am-shim-{}", crate::db::ulid()));
            std::fs::create_dir_all(&dir).unwrap();
            super::install_local(&dir).unwrap();
            let fake = dir.join("real");
            std::fs::create_dir_all(&fake).unwrap();
            // `agent get <name>` answers from `AM_TEST_AGENTS`; everything else echoes argv.
            // `agent start` 跟 herdr ≥0.9.0 一樣擋控制字元（`char::is_control`：換行、tab、ESC…，#772），整套測試都在驗 herdr 收得下。
            write_script(
                &fake.join("herdr"),
                "if [ \"$1\" = agent ] && [ \"$2\" = get ]; then\n\
                   case \" ${AM_TEST_AGENTS:-} \" in *\" $3 \"*) exit 0 ;; *) exit 1 ;; esac\n\
                 fi\n\
                 if [ \"$1\" = pane ] && [ \"$2\" = get ]; then\n\
                   [ -n \"${AM_TEST_PANE_JSON:-}\" ] || exit 1\n\
                   printf '%s\\n' \"$AM_TEST_PANE_JSON\"; exit 0\n\
                 fi\n\
                 if [ -n \"${AM_TEST_CREATE_JSON:-}\" ]; then printf '%s\\n' \"$AM_TEST_CREATE_JSON\"; exit 0; fi\n\
                 if [ \"$1\" = pane ] && [ \"$2\" = send-text ]; then\n\
                   { for a in \"$@\"; do printf '%s\\n' \"$a\"; done; printf -- '---\\n'; } >> \"${AM_TEST_SENDTEXT_LOG:-/dev/null}\"\n\
                   exit 0\n\
                 fi\n\
                 if [ \"$1\" = agent ] && [ \"$2\" = start ]; then\n\
                   for a in \"$@\"; do\n\
                     if [ \"$(printf '%s' \"$a\" | LC_ALL=C tr -d '\\000-\\037\\177')\" != \"$a\" ]; then\n\
                       printf '{\"error\":{\"code\":\"invalid_agent_argument\"}}\\n' >&2; exit 1\n\
                     fi\n\
                   done\n\
                 fi\n\
                 if [ -n \"${AM_TEST_SIGNAL_PARENT:-}\" ]; then kill -INT \"$PPID\"; sleep 2; exit 1; fi\n\
                 for a in \"$@\"; do printf '%s\\n' \"$a\"; done\n\
                 [ -z \"${AM_TEST_HERDR_RC:-}\" ] || exit \"$AM_TEST_HERDR_RC\"\n",
            );
            write_script(
                &fake.join("curl"),
                "case \"$*\" in\n\
                   */relay/spawn/begin*) printf '{\"permit_id\":\"test-permit\"}\\n200'; exit 0 ;;\n\
                   */relay/spawn/finish*) printf '{\"registered\":true}\\n200'; exit 0 ;;\n\
                   */relay/spawn/abort*) printf '{\"released\":true}\\n200'; exit 0 ;;\n\
                   *) exit 7 ;;\n\
                 esac\n",
            );
            Sandbox { dir }
        }

        fn run(&self, env: &[(&str, &str)], args: &[&str]) -> (Vec<String>, String) {
            let (out, err, _) = self.run_full(env, args);
            (out, err)
        }

        /// 假 curl（`real/curl`）：本體是 `body`。shim 的 `-w '\n%{http_code}'` 要靠它自己補狀態碼。
        fn install_fake_curl(&self, body: &str) {
            write_script(&self.dir.join("real").join("curl"), &format!("{body}\n"));
        }

        /// 同 [`Sandbox::run`]，另外回 shim 的結束碼。
        /// `pane send-text` 那一次呼叫（#389）：`(pane, 送進去的文字)`。env 補送一律只有一次呼叫。
        fn sent_text(&self) -> (String, String) {
            let sent = std::fs::read_to_string(self.dir.join("sendtext.log")).unwrap_or_default();
            let calls: Vec<&str> = sent.split("---\n").filter(|c| !c.trim().is_empty()).collect();
            assert_eq!(calls.len(), 1, "只補一次：{sent}");
            let lines: Vec<&str> = calls[0].lines().collect();
            assert_eq!(&lines[..2], ["pane", "send-text"], "{lines:?}");
            (lines[2].to_string(), lines[3..].join("\n"))
        }

        /// 只送一行 ` . '<檔>' && rm -f '<檔>'`，回暫存檔內容（檔案模式要是 0600）。
        fn sourced_env(&self) -> (String, String) {
            use std::os::unix::fs::PermissionsExt;
            let (pane, text) = self.sent_text();
            assert!(text.starts_with(" . '") && text.ends_with("\n") && text.matches('\n').count() == 1, "只送一行 source：{text:?}");
            let file = text.trim_end().strip_prefix(" . '").unwrap().split("' && rm -f '").next().unwrap().to_string();
            assert_eq!(text, format!(" . '{file}' && rm -f '{file}'\n"), "{text:?}");
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600, "暫存檔要 0600");
            (pane, std::fs::read_to_string(&file).unwrap())
        }

        fn run_full(&self, env: &[(&str, &str)], args: &[&str]) -> (Vec<String>, String, i32) {
            self.run_with_path(None, None, env, args)
        }

        /// 指定用哪個 shell 跑 shim（macOS 的 `/bin/sh`、`/bin/bash` 是 3.2，另有 `/bin/dash`）；`None` 照 shebang。
        fn run_in(&self, shell: Option<&str>, env: &[(&str, &str)], args: &[&str]) -> (Vec<String>, String, i32) {
            self.run_with_path(shell, None, env, args)
        }

        fn run_with_path(&self, shell: Option<&str>, path: Option<String>, env: &[(&str, &str)], args: &[&str]) -> (Vec<String>, String, i32) {
            let mut cmd = match shell {
                Some(sh) => {
                    let mut c = Command::new(sh);
                    c.arg(self.dir.join("bin/herdr"));
                    c
                }
                None => Command::new(self.dir.join("bin/herdr")),
            };
            let path = path.unwrap_or_else(|| {
                format!(
                    "{}:{}:/usr/bin:/bin",
                    self.dir.join("bin").display(),
                    self.dir.join("real").display()
                )
            });
            cmd.env("PATH", path).args(args);
            // Don't inherit the test runner's own pane model settings.
            // 在 bot 的 pane 裡跑測試時，AM_BOT_ID／AM_HOOK_TOKEN／AM_PORT 都有值，shim 會真的去打
            // 正在跑的 daemon——而雙角色上線之後，daemon 會把寫給 AGM 的那句攔進佇列、shim 不再轉給
            // herdr，測試就看到空輸出。需要這幾個值的測試自己設。
            // 字首清掉，不列清單：名單會漏（2026-09-19 `cargo_shim` 就是漏了 `AM_DAEMON_EXE`／
            // `AM_CONFIG_PATH`，在 bot pane 裡必定紅）。需要值的測試自己設。
            for (key, _) in std::env::vars() {
                if key.starts_with("AM_") {
                    cmd.env_remove(key);
                }
            }
            cmd.env_remove("HERDR_PANE_ID");
            // env 補送的暫存檔預設寫進沙盒，不要在 /tmp 留一堆（2026-10-01 一次測試留下幾十個 am-env.*）；測試自己給的 TMPDIR 照用。
            cmd.env("TMPDIR", &self.dir);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let out = output_retrying(&mut cmd);
            let stdout = String::from_utf8_lossy(&out.stdout).lines().map(String::from).collect();
            (stdout, String::from_utf8_lossy(&out.stderr).into_owned(), out.status.code().unwrap_or(-1))
        }
    }

    /// §6.5e：`--purpose` 是我們自己的旗標，不轉給 herdr；`tab create` 沒指定 workspace 時補專案的。
    #[test]
    fn pane_creation_strips_our_purpose_flag_and_defaults_the_workspace() {
        let s = Sandbox::new();
        let (out, _) = s.run(
            &[("AM_WORKSPACE_ID", "w1HJ"), ("AM_PROJECT_ID", "proj-1")],
            &["tab", "create", "--cwd", "/tmp", "--purpose", "dev-server"],
        );
        assert!(!out.iter().any(|a| a == "--purpose" || a == "dev-server"), "herdr 不認得這個旗標：{out:?}");
        assert!(out.windows(2).any(|w| w[0] == "--workspace" && w[1] == "w1HJ"), "{out:?}");
        assert!(out.windows(2).any(|w| w[0] == "--env" && w[1] == "AM_PROJECT_ID=proj-1"), "{out:?}");

        // 自己指定 workspace 就尊重，不補第二個。
        let (out, _) = s.run(
            &[("AM_WORKSPACE_ID", "w1HJ")],
            &["tab", "create", "--workspace", "w168", "--purpose=shell"],
        );
        assert_eq!(out.iter().filter(|a| *a == "--workspace").count(), 1, "{out:?}");
        assert!(out.iter().any(|a| a == "w168"), "{out:?}");
        assert!(!out.iter().any(|a| a.starts_with("--purpose")), "{out:?}");

        // `pane split` 以母 pane 為基準，本來就同 workspace：不補。
        let (out, _) = s.run(&[("AM_WORKSPACE_ID", "w1HJ")], &["pane", "split", "--pane", "w168:p1"]);
        assert!(!out.iter().any(|a| a == "--workspace"), "{out:?}");
    }

    /// review 2026-09-16 core 7：AM_WORKSPACE_ID 是 daemon 在決定 workspace 之前算的——第一次啟動／herdr 重開後沒有，
    /// 舊映射失效時是死掉的 id。`tab create` 先問 herdr 母 pane 現在在哪，問不到才用它；子 pane 繼承實際落點。
    #[test]
    fn a_new_tab_lands_in_the_parent_panes_live_workspace() {
        let s = Sandbox::new();
        let parent = r#"{"id":"cli:pane:get","result":{"pane":{"agent":"claude","cwd":"/p","pane_id":"w5:p1","tab_id":"w5:t1","workspace_id": "w5"},"type":"pane_info"}}"#;
        for stale in [&[][..], &[("AM_WORKSPACE_ID", "w1DEAD")][..]] {
            let mut env = vec![("HERDR_PANE_ID", "w5:p1"), ("AM_TEST_PANE_JSON", parent)];
            env.extend_from_slice(stale);
            let (out, _) = s.run(&env, &["tab", "create", "--cwd", "/tmp"]);
            assert!(out.windows(2).any(|w| w[0] == "--workspace" && w[1] == "w5"), "{stale:?} → {out:?}");
            assert_eq!(out.iter().filter(|a| *a == "--workspace").count(), 1, "{out:?}");
            assert_eq!(env_values(&out, "AM_WORKSPACE_ID"), vec!["w5".to_string()], "子 pane 繼承實際落點：{out:?}");
        }
        // herdr 問不到（母 pane 不在、舊 herdr）：退回 AM_WORKSPACE_ID。
        let (out, _) = s.run(&[("HERDR_PANE_ID", "w5:p1"), ("AM_WORKSPACE_ID", "w1HJ")], &["tab", "create"]);
        assert!(out.windows(2).any(|w| w[0] == "--workspace" && w[1] == "w1HJ"), "{out:?}");
    }

    /// 沒把握 2（review 2026-09-16）：用途回報取「herdr 輸出裡第一個 pane_id」。herdr 0.8.2 的 tab_created 只有
    /// `root_pane` 帶 pane_id（TabInfo 沒有），所以第一個就是新開的那顆；冒號後面有空白也要認得。
    #[test]
    fn the_purpose_is_reported_for_the_pane_that_was_just_created() {
        let s = Sandbox::new();
        let fake_curl = s.dir.join("real").join("curl");
        let log = s.dir.join("curl.log");
        write_script(
            &fake_curl,
            &format!(
                "printf '%s\\n' \"$@\" >> '{}'\n\
                 case \"$*\" in\n\
                   */relay/spawn/begin*) printf '{{\"permit_id\":\"test-permit\"}}\\n200'; exit 0 ;;\n\
                   */relay/spawn/finish*) printf '{{\"registered\":true}}\\n200'; exit 0 ;;\n\
                 esac\n",
                log.display()
            ),
        );
        let created = r#"{"id":"cli:tab:create","result":{"root_pane":{"agent":null,"pane_id": "w5:p9","tab_id":"w5:t4","workspace_id":"w5"},"tab":{"label":"sh","tab_id":"w5:t4","workspace_id":"w5"},"type":"tab_created"}}"#;
        let env = [("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1"), ("AM_TEST_CREATE_JSON", created)];
        let (out, _) = s.run(&env, &["tab", "create", "--workspace", "w5", "--purpose", "dev-server"]);
        assert_eq!(out, [created.to_string()], "herdr 的輸出原樣印出");
        let sent = std::fs::read_to_string(&log).unwrap();
        assert!(sent.lines().any(|l| l == "pane_id=w5:p9"), "{sent}");
        assert!(sent.lines().any(|l| l == "purpose=dev-server"), "{sent}");
    }

    #[test]
    fn a_refused_spawn_gate_never_invokes_herdr() {
        let s = Sandbox::new();
        s.install_fake_curl("printf '{\"reason\":\"credential_rotation_pending\"}\\n409");
        let created = r#"{"id":"pane.created","result":{"pane":{"pane_id":"w1:p-child"}}}"#;
        let env = [
            ("AM_BOT_ID", "b1"),
            ("AM_HOOK_TOKEN", "tok"),
            ("AM_PORT", "7788"),
            ("AM_TEST_CREATE_JSON", created),
        ];
        let (out, err, rc) = s.run_full(&env, &["pane", "split", "--pane", "w1:p-parent"]);
        assert!(out.is_empty(), "herdr must not run when the gate refuses: {out:?} {err}");
        assert_eq!(rc, 75, "a closed gate is an explicit retryable refusal: {err}");
        assert!(err.contains("沒有建立子 pane"), "{err}");
    }

    /// §6.5f：子 pane 寫的檔案也要落在母 bot 的 outbox，使用者才在同一個地方看得到。
    #[test]
    fn a_child_pane_inherits_the_outbox() {
        let s = Sandbox::new();
        let outbox = [("AM_OUTBOX", "/data/outbox/B1")];
        for argv in [&["pane", "split", "--pane", "w168:p1"][..], &["tab", "create", "--cwd", "/tmp"][..]] {
            let (out, _) = s.run(&outbox, argv);
            assert!(out.windows(2).any(|w| w[0] == "--env" && w[1] == "AM_OUTBOX=/data/outbox/B1"), "{argv:?} → {out:?}");
        }
        let (out, _) = s.run(&[], &["pane", "split", "--pane", "w168:p1"]);
        assert!(!out.iter().any(|a| a.starts_with("AM_OUTBOX=")), "母 pane 沒有就不帶：{out:?}");
    }

    /// issue #138：cargo shim 要把 check／test／clippy 轉到外部編譯主機，pane 裡得有 `AM_DAEMON_EXE` 與
    /// `AM_CONFIG_PATH`。daemon 起 bot 時會注入（`pane_env`），但 herdr 的 pane 是 **server** 生的、不繼承
    /// 呼叫端 shell，所以父 bot 用 `pane split`／`tab create` 開子 pane 時只有傳遞清單裡的 key 到得了——
    /// 這兩個當初（#104）沒進清單，於是每一個子 agent 的 cargo 都留在本機，沒有任何提示。
    #[test]
    fn a_child_pane_inherits_the_cargo_offload_helper_paths() {
        let s = Sandbox::new();
        let helper = [("AM_DAEMON_EXE", "/opt/am/agents-managerd"), ("AM_CONFIG_PATH", "/home/u/.config/agents-manager/config.toml")];
        for argv in [
            &["pane", "split", "--pane", "w168:p1"][..],
            &["pane", "new", "--cwd", "/tmp"][..],
            &["tab", "create", "--cwd", "/tmp"][..],
            &["workspace", "create", "--cwd", "/tmp"][..],
        ] {
            let (out, _) = s.run(&helper, argv);
            for (k, v) in helper {
                assert_eq!(env_values(&out, k), vec![v.to_string()], "{argv:?} 要把 {k} 傳給子 pane：{out:?}");
            }
        }
        // 母 pane 沒有就不帶（不要生出空值）。
        let (out, _) = s.run(&[], &["pane", "split", "--pane", "w168:p1"]);
        assert!(env_values(&out, "AM_DAEMON_EXE").is_empty() && env_values(&out, "AM_CONFIG_PATH").is_empty(), "{out:?}");
        // 呼叫者自己給了就尊重，不補第二份。
        let (out, _) = s.run(&helper, &["pane", "split", "--pane", "w168:p1", "--env", "AM_DAEMON_EXE=/mine/agents-managerd"]);
        assert_eq!(env_values(&out, "AM_DAEMON_EXE"), vec!["/mine/agents-managerd".to_string()], "{out:?}");
        assert_eq!(env_values(&out, "AM_CONFIG_PATH"), vec![helper[1].1.to_string()], "沒自己給的那個照母 pane：{out:?}");
    }

    /// `agent start` 沒有 `--env`：重用一顆沒走過 `pane split` 的舊 pane 時，靠 send-text 補 export
    /// （issue #57）。同一份清單，所以這兩個也要補，不然那顆 pane 裡的 cargo 一樣留在本機。
    #[test]
    fn agent_start_reexports_the_cargo_offload_helper_paths_too() {
        let s = Sandbox::new();
        let log = s.dir.join("sendtext.log");
        let env = [
            ("AM_AGENT_NAME", "p-1"),
            ("AM_DAEMON_EXE", "/opt/am/agents-managerd"),
            ("AM_CONFIG_PATH", "/home/u/.config/agents-manager/config.toml"),
            ("AM_TEST_SENDTEXT_LOG", log.to_str().unwrap()),
            ("TMPDIR", s.dir.to_str().unwrap()),
        ];
        s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9"]);
        let (_, sent) = s.sourced_env();
        assert!(sent.contains("export AM_DAEMON_EXE='/opt/am/agents-managerd'"), "{sent}");
        assert!(sent.contains("export AM_CONFIG_PATH='/home/u/.config/agents-manager/config.toml'"), "{sent}");
    }

    /// #138 的根因是兩份清單各改各的：daemon 注入什麼進 pane（`lifecycle/setup.rs` 的 `env.insert`），
    /// 跟 shim 把什麼傳給子 pane（`AM_RESERVED_ENV_KEYS`＋隔離實例那兩個）。#104 加了兩個 key、只改了前者。
    /// 這條把兩邊綁在一起：daemon 注入的每一個 key，要嘛在傳遞清單裡，要嘛明列成「刻意不傳」並寫理由——
    /// 下一個新增 pane 環境變數的人不改 shim 就會在這裡紅。
    #[test]
    fn every_env_key_the_daemon_injects_reaches_child_panes_or_is_deliberately_left_out() {
        // 刻意不傳。
        const LEFT_OUT: [(&str, &str); 2] = [
            ("CLAUDE_CODE_CHILD_SESSION", "空字串：daemon 用來洗掉它自己環境裡的 claude 標記；herdr server 開的子 pane 本來就沒有"),
            ("CLAUDECODE", "同上"),
        ];
        let listed: Vec<&str> = super::SHIM_SH
            .lines()
            .find_map(|l| l.strip_prefix("AM_RESERVED_ENV_KEYS=\"").and_then(|r| r.strip_suffix('"')))
            .expect("shim 裡要有 AM_RESERVED_ENV_KEYS")
            .split_whitespace()
            .collect();
        let setup_src = include_str!("lifecycle/setup.rs");
        let needle = concat!("env", ".insert(\"");
        let injected: std::collections::BTreeSet<&str> = setup_src
            .match_indices(needle)
            .filter_map(|(i, _)| setup_src[i + needle.len()..].split('"').next())
            .filter(|k| k.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
            .collect();
        assert!(injected.contains("AM_BOT_ID") && injected.contains("AM_DAEMON_EXE"), "解析 setup.rs 失敗：{injected:?}");
        for key in injected {
            let passed_down = listed.contains(&key) || matches!(key, "AM_INSTANCE" | "AM_DATA_DIR");
            let left_out = LEFT_OUT.iter().any(|(k, _)| *k == key);
            assert!(passed_down || left_out, "daemon 會把 {key} 注入 bot 的 pane，但 herdr shim 開子 pane 時不會傳它——加進 AM_RESERVED_ENV_KEYS，或列進 LEFT_OUT 並寫理由（issue #138）");
        }
    }

    #[test]
    fn agent_start_prefixes_the_child_name() {
        let s = Sandbox::new();
        let (out, err) =
            s.run(&[("AM_AGENT_NAME", "proj-abc123")], &["agent", "start", "review", "--kind", "claude"]);
        assert_eq!(out, ["agent", "start", "proj-abc123-review", "--kind", "claude"]);
        assert!(err.contains("proj-abc123-review"), "the rename is announced: {err}");
    }

    /// 回報 daemon 失敗（測試裡沒有 daemon）也不能擋住轉發。
    #[test]
    fn agent_prompt_prefixes_the_target_and_forwards_the_text() {
        let s = Sandbox::new();
        let (out, _) = s.run(
            &[("AM_AGENT_NAME", "proj-abc123")],
            &["agent", "prompt", "review", "把 daemon 重建一次，然後回報"],
        );
        // 整段文字仍是**一個**參數。
        assert_eq!(out, ["agent", "prompt", "proj-abc123-review", "把 daemon 重建一次，然後回報"]);
    }

    /// 2026-09-30：遠端 bot（沒有 `AM_PORT`）直送的一句，要在打字前寫一則報備進自己的 hook spool，
    /// daemon 收 hook 時才認得出寄件者。沒有 bot 目錄就什麼都不寫，照舊轉發。
    #[test]
    fn a_remote_bot_spools_a_relay_announce_before_prompting() {
        let s = Sandbox::new();
        let home = s.dir.join("home");
        let spool = home.join(".config/agents-manager/bots/B-REMOTE/hook-spool.d");
        std::fs::create_dir_all(spool.parent().unwrap()).unwrap();
        let home_s = home.to_string_lossy().to_string();
        let env = [("HOME", home_s.as_str()), ("AM_BOT_ID", "B-REMOTE"), ("AM_KIND", "claude"), ("AM_TEST_AGENTS", "robins-hub-3b84sb")];
        let (out, _) = s.run(&env, &["agent", "prompt", "robins-hub-3b84sb", "我是 robins-hub-bf3xq3。PR #95 卡在排隊", "--wait"]);
        assert_eq!(out, ["agent", "prompt", "robins-hub-3b84sb", "我是 robins-hub-bf3xq3。PR #95 卡在排隊", "--wait"], "照舊轉發");
        let files: Vec<_> = std::fs::read_dir(&spool).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        assert_eq!(files.len(), 1, "一則報備：{files:?}");
        let body: serde_json::Value = serde_json::from_str(std::fs::read_to_string(&files[0]).unwrap().trim()).unwrap();
        assert_eq!(body["bot_id"], "B-REMOTE");
        assert_eq!(body["provider"], "claude");
        assert_eq!(body["payload"]["hook_event_name"], "AmRelayAnnounce");
        assert_eq!(body["payload"]["to_agent"], "robins-hub-3b84sb");
        assert_eq!(body["payload"]["text"], "我是 robins-hub-bf3xq3。PR #95 卡在排隊", "--wait 不進正文");
        let at = body["received_at"].as_str().unwrap();
        assert!(crate::db::parse_ts(at).is_some() && at.len() == "2026-09-30T04:23:28.123Z".len() && at.ends_with('Z'), "毫秒 UTC 格式：{at}");

        // 本機 bot（有 AM_PORT）不走 spool；沒有 bot 目錄的也不寫。
        let s2 = Sandbox::new();
        let home2 = s2.dir.join("home");
        let home2_s = home2.to_string_lossy().to_string();
        std::fs::create_dir_all(&home2).unwrap();
        let (out, _) = s2.run(&[("HOME", home2_s.as_str()), ("AM_BOT_ID", "B-NODIR")], &["agent", "prompt", "x-agent", "hello there relay"]);
        assert_eq!(out[..2], ["agent", "prompt"]);
        assert!(!home2.join(".config").exists(), "沒有 bot 目錄就不建");
    }

    /// 既有目標（AGM、頂層 bot）不改名，否則 unknown_target。
    #[test]
    fn an_existing_target_is_prompted_under_its_own_name() {
        let s = Sandbox::new();
        let env = [("AM_AGENT_NAME", "proj-abc123"), ("AM_TEST_AGENTS", "agm-pxf2pv proj-abc123-review")];
        let (out, err) = s.run(&env, &["agent", "prompt", "agm-pxf2pv", "請准我重啟 daemon"]);
        assert_eq!(out, ["agent", "prompt", "agm-pxf2pv", "請准我重啟 daemon"]);
        assert!(!err.contains("已改名"), "{err}");
        let (out, _) = s.run(&env, &["agent", "prompt", "review", "hi"]);
        assert_eq!(out, ["agent", "prompt", "proj-abc123-review", "hi"]);
    }

    /// daemon 說「排進協調佇列了」：不再轉給真的 herdr（那會打進 AGM 的 pane、燒巡檢一回合）。
    /// daemon 沒說（一般 bot、舊部署、daemon 不在）就照舊轉發。
    #[test]
    fn a_request_the_daemon_queued_for_agm_is_not_typed_into_its_pane() {
        let s = Sandbox::new();
        let fake_curl = s.dir.join("real").join("curl");
        write_script(&fake_curl, "printf '%s\\n%s' \"${AM_TEST_CURL_REPLY:-}\" \"${AM_TEST_CURL_CODE:-200}\"\n");
        let base = [("AM_AGENT_NAME", "proj-abc123"), ("AM_TEST_AGENTS", "agm-pxf2pv"), ("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1")];
        let mut env = base.to_vec();
        env.push(("AM_TEST_CURL_REPLY", r#"{"routed":"responder","inbox_event_id":"e1"}"#));
        let (out, err) = s.run(&env, &["agent", "prompt", "agm-pxf2pv", "請准我重啟 daemon"]);
        assert!(out.is_empty(), "真的 herdr 沒被叫到：{out:?}");
        assert!(err.contains("協調佇列") && err.contains("e1"), "{err}");

        let mut env = base.to_vec();
        env.push(("AM_TEST_CURL_REPLY", "{}"));
        let (out, _) = s.run(&env, &["agent", "prompt", "agm-pxf2pv", "請准我重啟 daemon"]);
        assert_eq!(out, ["agent", "prompt", "agm-pxf2pv", "請准我重啟 daemon"]);
    }

    /// daemon 說「這是寫給 AGM 的，但現在排不進佇列」（issue #143，5xx＋`routing_unavailable`）：不能照舊直送——
    /// 那會繞過協調佇列、打進 AGM 的 pane。明確失敗，讓寄件的 bot 自己重試。
    #[test]
    fn a_request_the_daemon_could_not_route_is_not_typed_into_the_pane() {
        let s = Sandbox::new();
        let fake_curl = s.dir.join("real").join("curl");
        write_script(&fake_curl, "printf '%s\\n%s' \"${AM_TEST_CURL_REPLY:-}\" \"${AM_TEST_CURL_CODE:-200}\"\n");
        let mut env = vec![("AM_AGENT_NAME", "proj-abc123"), ("AM_TEST_AGENTS", "agm-pxf2pv"), ("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1")];
        env.push(("AM_TEST_CURL_REPLY", r#"{"error":"routing_unavailable","routing_unavailable":true,"retryable":true}"#));
        env.push(("AM_TEST_CURL_CODE", "503"));
        let (out, err) = s.run(&env, &["agent", "prompt", "agm-pxf2pv", "請准我重啟 daemon"]);
        assert!(out.is_empty(), "排不進協調佇列不能退回直送：{out:?}");
        assert!(err.contains("routing_unavailable") || err.contains("稍後重試"), "{err}");
    }

    /// `--ack`／`--reply-to` 是我們的旗標（review 2026-09-16 H1：寄件端明講才算回覆）：送給 daemon、不給真的
    /// herdr。herdr 自己的 `--wait --timeout` 不算正文。curl 逾時（28）先再問一次，不直接打進 AGM 的 pane。
    #[test]
    fn reply_marks_go_to_the_daemon_and_a_slow_daemon_is_asked_again_before_falling_back() {
        let s = Sandbox::new();
        let log = s.dir.join("curl.log");
        let count = s.dir.join("curl.count");
        let fake_curl = s.dir.join("real").join("curl");
        // 每次呼叫把 --data-urlencode 的值一行一行記下來；第一次照 AM_TEST_CURL_FIRST_RC 結束。
        write_script(
            &fake_curl,
            &format!(
                "n=$(cat '{count}' 2>/dev/null || echo 0); n=$((n + 1)); echo $n > '{count}'\n\
                 prev=''\n\
                 for a in \"$@\"; do [ \"$prev\" = --data-urlencode ] && printf '%s\\n' \"$a\" >> '{log}'; prev=$a; done\n\
                 echo --- >> '{log}'\n\
                 if [ \"$n\" = 1 ] && [ -n \"${{AM_TEST_CURL_FIRST_RC:-}}\" ]; then exit \"$AM_TEST_CURL_FIRST_RC\"; fi\n\
                 printf '%s\\n%s' \"${{AM_TEST_CURL_REPLY:-}}\" \"${{AM_TEST_CURL_CODE:-200}}\"\n",
                count = count.display(),
                log = log.display(),
            ),
        );
        let base = [("AM_AGENT_NAME", "proj-abc123"), ("AM_TEST_AGENTS", "agm-pxf2pv"), ("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1")];

        // daemon 沒排進佇列（`{}`）→ 照舊直送，但我們的旗標不給 herdr，herdr 的旗標原樣保留。
        let mut env = base.to_vec();
        env.push(("AM_TEST_CURL_REPLY", "{}"));
        let (out, _) = s.run(&env, &["agent", "prompt", "agm-pxf2pv", "收到", "--ack", "--reply-to", "ev-1", "--wait", "--timeout", "5000"]);
        assert_eq!(out, ["agent", "prompt", "agm-pxf2pv", "收到", "--wait", "--timeout", "5000"]);
        let sent = std::fs::read_to_string(&log).unwrap();
        assert!(sent.contains("text=收到\n"), "正文不含 herdr 的旗標：{sent}");
        assert!(sent.contains("ack=1\n") && sent.contains("reply_to=ev-1\n"), "{sent}");

        // 沒帶旗標：ack／reply_to 送空的（daemon 當成新的事、叫醒）。
        std::fs::remove_file(&log).unwrap();
        std::fs::remove_file(&count).unwrap();
        let (_, _) = s.run(&env, &["agent", "prompt", "agm-pxf2pv", "請核准重啟"]);
        let sent = std::fs::read_to_string(&log).unwrap();
        assert!(sent.contains("ack=\n") && sent.contains("reply_to=\n"), "{sent}");

        // 第一次逾時、第二次 daemon 說排進佇列了：不打進 pane。
        std::fs::remove_file(&log).unwrap();
        std::fs::remove_file(&count).unwrap();
        let mut env = base.to_vec();
        env.push(("AM_TEST_CURL_FIRST_RC", "28"));
        env.push(("AM_TEST_CURL_REPLY", r#"{"routed":"responder","inbox_event_id":"e9"}"#));
        let (out, err) = s.run(&env, &["agent", "prompt", "agm-pxf2pv", "請核准重啟"]);
        assert!(out.is_empty(), "逾時後再問到了，真的 herdr 不該被叫到：{out:?}");
        assert!(err.contains("e9"), "{err}");
        assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "2");

        // 連不上（7）不重問，照舊直送。
        std::fs::remove_file(&count).unwrap();
        let mut env = base.to_vec();
        env.push(("AM_TEST_CURL_FIRST_RC", "7"));
        let (out, _) = s.run(&env, &["agent", "prompt", "agm-pxf2pv", "請核准重啟"]);
        assert_eq!(out, ["agent", "prompt", "agm-pxf2pv", "請核准重啟"]);
        assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "1");
    }

    /// 這台機器上有的 shell（沒有的略過）。macOS 的 `/bin/sh`／`/bin/bash` 是 3.2——腳本改動要在那個版本也驗。
    fn shells() -> Vec<&'static str> {
        ["/bin/sh", "/bin/bash", "/bin/dash"].into_iter().filter(|p| std::path::Path::new(p).exists()).collect()
    }

    /// issue #143（重開）：daemon 有回、但不是「排進佇列」也不是明確的 `{}`——非 2xx（400／401／403／404／5xx，body 沒有 `routing_unavailable`）、
    /// 或 2xx 卻是看不懂的內容。以前一律退回直送 pane：佇列的申請可能已經 commit，直送就是佇列一份、pane 一份；401／403 更是把認證失敗變成繞過控制面的旁路。
    /// 現在一律不直送、明確失敗（75；身分被拒 77）。每一種都驗真的 herdr 沒被叫到 `agent prompt`。
    #[test]
    fn a_daemon_that_refuses_or_answers_nonsense_never_gets_a_direct_prompt() {
        let cases: [(&str, &str, i32); 9] = [
            ("400", r#"{"error":"bad form"}"#, 75),
            ("401", r#"{"error":"unknown bot or bad token"}"#, 77),
            ("403", r#"{"error":"forbidden"}"#, 77),
            ("404", "", 75),
            ("500", r#"{"error":"boom"}"#, 75),
            ("502", "<html>Bad Gateway</html>", 75),
            ("503", r#"{"error":"busy"}"#, 75),
            ("200", "<html>hello</html>", 75),
            ("200", "", 75),
        ];
        for (sh, (code, reply, want_rc)) in shells().into_iter().flat_map(|sh| cases.into_iter().map(move |c| (sh, c))) {
            let s = Sandbox::new();
            s.install_fake_curl("printf '%s\\n%s' \"${AM_TEST_CURL_REPLY:-}\" \"${AM_TEST_CURL_CODE:-200}\"");
            let env = [
                ("AM_AGENT_NAME", "proj-abc123"),
                ("AM_TEST_AGENTS", "agm-pxf2pv"),
                ("AM_BOT_ID", "b1"),
                ("AM_HOOK_TOKEN", "tok"),
                ("AM_PORT", "1"),
                ("AM_TEST_CURL_CODE", code),
                ("AM_TEST_CURL_REPLY", reply),
            ];
            let (out, err, rc) = s.run_in(Some(sh), &env, &["agent", "prompt", "agm-pxf2pv", "請准我重啟 daemon"]);
            let why = format!("{sh} HTTP {code} {reply:?}: {err}");
            assert!(out.is_empty(), "不能退回直送 pane：{why} {out:?}");
            assert_eq!(rc, want_rc, "{why}");
            assert!(err.contains("沒有送出"), "要講明沒送出：{why}");
        }
    }

    /// issue #143（重開）：「送了但回覆不明」——逾時（28）、空回應（52）、連線被重置（56）、送到一半（55）、傳輸中斷（18）：request 可能已進 daemon。
    /// 同一個請求再問一次（daemon 以內容指紋去重，不會變兩筆），還是不明就明確失敗，**絕不直送**；再問到了就照 daemon 說的做。
    /// 只有 curl 7（連線根本沒建立，request 一定沒到 daemon）才照舊直送（既有測試）。
    #[test]
    fn an_ambiguous_transport_failure_is_asked_again_and_then_stops_instead_of_typing_into_the_pane() {
        for (sh, rc) in shells().into_iter().flat_map(|sh| [28, 52, 56, 55, 18].into_iter().map(move |rc| (sh, rc))) {
            let s = Sandbox::new();
            let count = s.dir.join("curl.count");
            s.install_fake_curl(&format!(
                "n=$(cat '{c}' 2>/dev/null || echo 0); n=$((n + 1)); echo $n > '{c}'\n\
                 if [ -n \"${{AM_TEST_CURL_RC:-}}\" ] && [ \"$n\" -le \"${{AM_TEST_CURL_FAILS:-99}}\" ]; then exit \"$AM_TEST_CURL_RC\"; fi\n\
                 printf '%s\\n%s' \"${{AM_TEST_CURL_REPLY:-}}\" \"${{AM_TEST_CURL_CODE:-200}}\"",
                c = count.display()
            ));
            let rc_s = rc.to_string();
            let base = [("AM_AGENT_NAME", "proj-abc123"), ("AM_TEST_AGENTS", "agm-pxf2pv"), ("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1")];

            // 一直不明：問兩次就停，不直送。
            let mut env = base.to_vec();
            env.push(("AM_TEST_CURL_RC", &rc_s));
            let (out, err, code) = s.run_in(Some(sh), &env, &["agent", "prompt", "agm-pxf2pv", "請准我重啟 daemon"]);
            assert!(out.is_empty(), "{sh} curl {rc}：回覆不明不能直送：{out:?} {err}");
            assert_eq!(code, 75, "curl {rc}: {err}");
            assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "2", "同一個請求問兩次（第二次給久一點）");
            assert!(err.contains(&format!("curl 結束碼 {rc}")) && err.contains("沒有送出"), "{err}");

            // 第一次不明、再問到了（daemon 說排進佇列）：照 daemon 說的，不直送。
            std::fs::remove_file(&count).unwrap();
            let mut env = base.to_vec();
            env.push(("AM_TEST_CURL_RC", &rc_s));
            env.push(("AM_TEST_CURL_FAILS", "1"));
            env.push(("AM_TEST_CURL_REPLY", r#"{"routed":"responder","inbox_event_id":"e7"}"#));
            let (out, err, code) = s.run_in(Some(sh), &env, &["agent", "prompt", "agm-pxf2pv", "請准我重啟 daemon"]);
            assert!(out.is_empty() && code == 0, "{sh} curl {rc}：再問到了就照佇列：{out:?} {code} {err}");
        }
    }

    /// 受管的 bot（有 bot 身分與 `AM_PORT`）而這台機器沒有 curl：問不了 daemon 這句要不要走佇列——一樣不直送，明確失敗。
    /// 沒有 bot 身分的人工 shell 照舊直送（daemon 根本不知道它）。
    #[test]
    fn a_managed_bot_without_curl_does_not_type_into_the_pane() {
        let s = Sandbox::new();
        std::fs::remove_file(s.dir.join("real/curl")).unwrap();
        let tools = s.dir.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        for t in ["tr", "sed", "cut", "dirname", "head", "tail", "cat", "env", "sh"] {
            for base in ["/usr/bin", "/bin"] {
                let src = std::path::Path::new(base).join(t);
                if src.exists() {
                    let _ = std::os::unix::fs::symlink(&src, tools.join(t));
                    break;
                }
            }
        }
        let path = format!("{}:{}:{}", s.dir.join("bin").display(), s.dir.join("real").display(), tools.display());
        let managed = [("AM_AGENT_NAME", "proj-abc123"), ("AM_TEST_AGENTS", "agm-pxf2pv"), ("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1")];
        let (out, err, rc) = s.run_with_path(None, Some(path.clone()), &managed, &["agent", "prompt", "agm-pxf2pv", "請准我重啟 daemon"]);
        assert!(out.is_empty(), "{out:?} {err}");
        assert_eq!(rc, 75, "{err}");
        assert!(err.contains("沒有 curl"), "{err}");
        // 人工 shell：沒有 bot 身分，直送。
        let (out, _, rc) = s.run_with_path(None, Some(path), &[("AM_TEST_AGENTS", "agm-pxf2pv")], &["agent", "prompt", "agm-pxf2pv", "hi"]);
        assert_eq!((rc, out), (0, vec!["agent".to_string(), "prompt".into(), "agm-pxf2pv".into(), "hi".into()]));
    }

    /// issue #143 的回歸（重開）：daemon **已經**把申請 durable 寫進協調者的收件匣，回覆卻在路上斷掉（curl 52，empty reply）——
    /// shim 不能因此直送 pane（佇列一份、pane 一份）；重問同一句（內容指紋去重）收件匣仍只有一筆；回覆通了之後照佇列說的做。
    /// **真的** router（跟 daemon 同一份）＋**真的** curl＋**真的** shim；中間一個 TCP proxy 把 daemon 的回覆吞掉。
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_reply_lost_after_the_request_was_queued_is_never_answered_with_a_direct_prompt() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::{TcpListener, TcpStream};

        let app = crate::supervisor::bot_requests::flow_tests::app().await;
        crate::supervisor::bot_requests::flow_tests::configure_responder(&app).await;
        let router_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let router_port = router_l.local_addr().unwrap().port();
        router_l.set_nonblocking(true).unwrap();
        let router = crate::api::router(app.clone());
        let server = tokio::spawn(async move {
            let l = TcpListener::from_std(router_l).unwrap();
            let _ = axum::serve(l, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
        });

        let drop_reply = Arc::new(AtomicBool::new(true));
        let seen = Arc::new(AtomicUsize::new(0));
        let proxy_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_l.local_addr().unwrap().port();
        let (dr, sn) = (drop_reply.clone(), seen.clone());
        let proxy = tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = proxy_l.accept().await else { return };
                let (dr, sn) = (dr.clone(), sn.clone());
                tokio::spawn(async move {
                    // 讀完整個 request（標頭＋Content-Length 的本文）。
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let (head_end, want) = loop {
                        let n = client.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                            let cl = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                            break (pos + 4, cl);
                        }
                    };
                    while buf.len() < head_end + want {
                        let n = client.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    sn.fetch_add(1, Ordering::SeqCst);
                    // 轉給真的 router（要它關連線，才讀得到整個回覆）。
                    let mut req = buf[..head_end - 2].to_vec();
                    req.extend_from_slice(b"Connection: close\r\n\r\n");
                    req.extend_from_slice(&buf[head_end..]);
                    let Ok(mut up) = TcpStream::connect(("127.0.0.1", router_port)).await else { return };
                    let _ = up.write_all(&req).await;
                    let mut resp = Vec::new();
                    let _ = up.read_to_end(&mut resp).await;
                    if dr.load(Ordering::SeqCst) {
                        return; // request 已經處理完、daemon 也回了，但回覆不轉給 curl：它看到 empty reply。
                    }
                    let _ = client.write_all(&resp).await;
                });
            }
        });

        let s = Sandbox::new();
        // This test exercises a real TCP proxy; use the machine's curl, not the sandbox fake
        // that answers only the spawn-permit endpoints.
        std::fs::remove_file(s.dir.join("real/curl")).unwrap();
        let port = proxy_port.to_string();
        let text = "請核准重建 relay-143-lost-reply";
        let run = |s: Sandbox, port: String| {
            tokio::task::spawn_blocking(move || {
                let env = [("AM_AGENT_NAME", "fixer-abc"), ("AM_TEST_AGENTS", "AGM"), ("AM_BOT_ID", "w1"), ("AM_HOOK_TOKEN", "tok-w1"), ("AM_PORT", port.as_str())];
                let r = s.run_full(&env, &["agent", "prompt", "AGM", text]);
                (s, r)
            })
        };
        let inbox = || async { sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='bot_request'").fetch_one(&app.db).await.unwrap() };

        // 回覆一直斷：request 進了 daemon（收件匣一筆），shim 問兩次後不直送、明確失敗。
        let (s, (out, err, rc)) = run(s, port.clone()).await.unwrap();
        assert!(out.is_empty(), "回覆丟了不能直送 pane（佇列一份、pane 一份）：{out:?} {err}");
        assert_eq!(rc, 75, "{err}");
        assert_eq!(seen.load(Ordering::SeqCst), 2, "同一個請求問了兩次：{err}");
        assert_eq!(inbox().await, 1, "兩次都進了 daemon，內容指紋去重後仍只有一筆 durable 申請");

        // 回覆通了：daemon 說已排進佇列，shim 不直送、exit 0；收件匣還是一筆。
        drop_reply.store(false, Ordering::SeqCst);
        let (_s, (out, err, rc)) = run(s, port).await.unwrap();
        assert!(out.is_empty() && rc == 0, "{out:?} {rc} {err}");
        assert!(err.contains("協調佇列"), "{err}");
        assert_eq!(inbox().await, 1, "重問不會變兩筆");
        proxy.abort();
        server.abort();
    }

    #[test]
    fn an_agent_prompt_with_flags_first_is_forwarded_verbatim() {
        let s = Sandbox::new();
        let (out, _) = s.run(&[("AM_AGENT_NAME", "proj-abc123")], &["agent", "prompt", "--json", "review", "hi"]);
        assert_eq!(out, ["agent", "prompt", "--json", "review", "hi"]);
    }

    #[test]
    fn a_prefixed_name_and_the_argv_order_are_left_alone() {
        let s = Sandbox::new();
        let (out, err) =
            s.run(&[("AM_AGENT_NAME", "proj-abc123")], &["agent", "start", "proj-abc123-ui", "--kind", "codex"]);
        assert_eq!(out, ["agent", "start", "proj-abc123-ui", "--kind", "codex"]);
        assert_eq!(err, "");

        let (out, _) = s.run(
            &[("AM_AGENT_NAME", "p-1")],
            &["agent", "start", "--kind", "claude", "--pane", "w1:p3", "ui", "--", "--model", "opus"],
        );
        assert_eq!(out, ["agent", "start", "--kind", "claude", "--pane", "w1:p3", "p-1-ui", "--", "--model", "opus"]);
    }

    /// clap 也收 `--kind=codex`／`--pane=ID`：等號拼法要跟空白拼法一樣處理。以前 `_kind`／`_pane` 只認空白拼法，
    /// `--kind=codex` 的子 agent 被當成「沒指定 kind」而補上母 bot（claude）的 `--model`，指示旗標也照母 bot 的 kind 補；
    /// `--pane=ID` 則被當成沒有 `--pane`，bot 的 agent start 直接被擋、也繞過「不能指到自己的 pane」的檢查。
    #[test]
    fn the_equals_spelling_of_kind_and_pane_is_understood() {
        let s = Sandbox::new();
        let env = [("AM_AGENT_NAME", "p-1"), ("AM_KIND", "claude"), ("AM_MODEL", "opus"), ("AM_EFFORT", "medium")];
        let (out, _) = s.run(&env, &["agent", "start", "kid", "--kind=codex"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--kind=codex"], "別的 kind 不借母 bot 的模型：{out:?}");

        let bot = [("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1"), ("AM_AGENT_NAME", "p-1")];
        let (_, err, rc) = s.run_full(&bot, &["agent", "start", "kid", "--kind", "claude", "--pane=w1:p3"]);
        assert_eq!(rc, 0, "有 --pane 的等號拼法不能被當成沒給：{err}");
        let mut own = bot.to_vec();
        own.push(("HERDR_PANE_ID", "w1:p1"));
        let (_, err, rc) = s.run_full(&own, &["agent", "start", "kid", "--kind", "claude", "--pane=w1:p1"]);
        assert_eq!(rc, 2, "指到自己的 pane 要擋：{err}");
    }

    /// `agent start --timeout` 要原樣帶給 begin（`timeout_ms`），permit 才撐得過那麼久；沒帶就是空字串（daemon 用預設）。
    #[test]
    fn begin_carries_the_agent_start_timeout() {
        for (args, want) in [
            (vec!["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p3", "--timeout", "120000"], "timeout_ms=120000"),
            (vec!["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p3", "--timeout=90000"], "timeout_ms=90000"),
            (vec!["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p3"], "timeout_ms="),
        ] {
            let s = Sandbox::new();
            let log = s.dir.join("curl.log");
            write_script(
                &s.dir.join("real").join("curl"),
                &format!(
                    "printf '%s\\n' \"$@\" >> '{}'\n\
                     case \"$*\" in\n\
                       */relay/spawn/begin*) printf '{{\"permit_id\":\"test-permit\"}}\\n200'; exit 0 ;;\n\
                       */relay/spawn/finish*) printf '{{\"registered\":true}}\\n200'; exit 0 ;;\n\
                     esac\n",
                    log.display()
                ),
            );
            let env = [("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1"), ("AM_AGENT_NAME", "p-1")];
            let (_, err, rc) = s.run_full(&env, &args);
            assert_eq!(rc, 0, "{err}");
            let sent = std::fs::read_to_string(&log).unwrap();
            assert!(sent.lines().any(|l| l == want), "{args:?} → {sent}");
            if want != "timeout_ms=" {
                // `--timeout` 的值不能被當成 agent 名字改名。
                assert!(!sent.contains("p-1-120000") && !sent.contains("p-1-90000"), "{sent}");
            }
        }
    }

    /// 2026-09-08：沒帶 `--model` 的子 agent 跑 CLI 預設；同 kind 才補母 bot 的，自己有寫的不動。
    #[test]
    fn a_child_without_a_model_inherits_the_parents() {
        let s = Sandbox::new();
        let env = [("AM_AGENT_NAME", "p-1"), ("AM_KIND", "claude"), ("AM_MODEL", "opus"), ("AM_EFFORT", "medium")];
        let (out, err) = s.run(&env, &["agent", "start", "kid", "--kind", "claude"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--kind", "claude", "--", "--model", "opus", "--effort", "medium"]);
        assert!(err.contains("沿用母 bot"), "{err}");

        let (out, _) = s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--", "--model", "sonnet"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--kind", "claude", "--", "--model", "sonnet"]);

        let (out, _) = s.run(&env, &["agent", "start", "kid", "--kind", "codex"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--kind", "codex"]);

        let cenv = [("AM_AGENT_NAME", "p-1"), ("AM_KIND", "codex"), ("AM_MODEL", "gpt-5.6-luna")];
        let (out, _) = s.run(&cenv, &["agent", "start", "kid", "--kind", "codex", "--", "-m", "o3"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--kind", "codex", "--", "-m", "o3"]);
        let (out, _) = s.run(&cenv, &["agent", "start", "kid", "--", "-c", "model=o3"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--", "-c", "model=o3"]);
        let (out, _) = s.run(&cenv, &["agent", "start", "kid", "--", "-m=o3"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--", "-m=o3"]);

        let (out, _) = s.run(&env, &["agent", "start", "kid", "--", "--verbose"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--", "--verbose", "--model", "opus", "--effort", "medium"]);
    }

    #[test]
    fn an_option_value_is_never_mistaken_for_the_name() {
        let s = Sandbox::new();
        let (out, _) = s.run(&[("AM_AGENT_NAME", "p-1")], &["agent", "start", "--pane", "w1:p3", "kid"]);
        assert_eq!(out, ["agent", "start", "--pane", "w1:p3", "p-1-kid"]);
    }

    #[test]
    fn a_long_name_is_cut_to_herdrs_32_characters() {
        let s = Sandbox::new();
        let parent = "proj-abc123";
        // POSIX cksum(40 個 x) % 1_000_000 = 320016；40 個 y → 890150。放不下就用這個尾碼，不是截前綴。
        let (out, _) = s.run(&[("AM_AGENT_NAME", parent)], &["agent", "start", &"x".repeat(40)]);
        assert_eq!(out[2], "proj-abc123-320016");
        assert_ne!(out[2], parent);
        assert!(out[2].len() <= 32);
        let (other, _) = s.run(&[("AM_AGENT_NAME", parent)], &["agent", "start", &"y".repeat(40)]);
        assert_eq!(other[2], "proj-abc123-890150");
        assert_ne!(other[2], parent);
        assert!(other[2].len() <= 32);
    }

    #[test]
    fn a_new_pane_carries_the_parents_account_and_hook_env() {
        let s = Sandbox::new();
        let (out, _) = s.run(
            &[
                ("AM_AGENT_NAME", "p-1"),
                ("AM_BOT_ID", "b1"),
                ("AM_HOOK_TOKEN", "tok"),
                ("AM_PORT", "7788"),
                ("CLAUDE_CONFIG_DIR", "/home/u/.claude-cc2"),
            ],
            &["pane", "split", "--pane", "w1:p1", "--direction", "right"],
        );
        assert_eq!(&out[..6], ["pane", "split", "--pane", "w1:p1", "--direction", "right"]);
        for want in ["CLAUDE_CONFIG_DIR=/home/u/.claude-cc2", "AM_BOT_ID=b1", "AM_HOOK_TOKEN=tok", "AM_PORT=7788"] {
            assert!(out.contains(&want.to_string()), "{want} was not passed down: {out:?}");
        }
    }

    #[test]
    fn an_env_the_caller_set_is_not_overridden() {
        let s = Sandbox::new();
        let (out, _) = s.run(
            &[("CLAUDE_CONFIG_DIR", "/home/u/.claude")],
            &["tab", "create", "--workspace", "w1", "--env", "CLAUDE_CONFIG_DIR=/other"],
        );
        assert_eq!(out.iter().filter(|a| a.starts_with("CLAUDE_CONFIG_DIR=")).count(), 1);
        assert!(out.contains(&"CLAUDE_CONFIG_DIR=/other".to_string()));
    }

    /// `--env=KEY=V` 也是呼叫者設過了。以前比對的是整串 argv 裡有沒有「空白＋KEY=」，
    /// 這種拼法前面是 `=`，於是同一個 key 被補第二份——子 agent 可能跑在母 bot 的帳號下。
    #[test]
    fn the_equals_spelling_also_counts_as_the_caller_setting_it() {
        let s = Sandbox::new();
        let (out, _) = s.run(
            &[("CLAUDE_CONFIG_DIR", "/home/u/.claude")],
            &["tab", "create", "--workspace", "w1", "--env=CLAUDE_CONFIG_DIR=/other"],
        );
        assert_eq!(env_values(&out, "CLAUDE_CONFIG_DIR"), vec!["/other".to_string()], "{out:?}");
    }

    /// 別的參數的值裡剛好有「 KEY=」不該讓那個 env 消失：以前比對整串 argv，
    /// `--label 'run with AM_PORT=x'` 會讓 AM_PORT 整個不被傳下去。
    #[test]
    fn a_label_that_mentions_a_key_does_not_swallow_that_env() {
        let s = Sandbox::new();
        let (out, _) = s.run(
            &[("AM_PORT", "7788")],
            &["tab", "create", "--workspace", "w1", "--label", "run with AM_PORT=x"],
        );
        assert_eq!(env_values(&out, "AM_PORT"), vec!["7788".to_string()], "{out:?}");
    }

    /// `--env KEY=V`／`--env=KEY=V` 裡某個 key 的所有值（只看 `--` 之前）。
    fn env_values(argv: &[String], key: &str) -> Vec<String> {
        let head = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
        let argv = &argv[..head];
        argv.iter()
            .enumerate()
            .filter_map(|(i, a)| {
                let v = a.strip_prefix("--env=").map(String::from).or_else(|| (i > 0 && argv[i - 1] == "--env").then(|| a.clone()))?;
                v.strip_prefix(&format!("{key}=")).map(String::from)
            })
            .collect()
    }

    const PARENTS: [&[(&str, &str)]; 2] = [
        &[("AM_AGENT_NAME", "p-1")],
        &[("AM_AGENT_NAME", "p-1"), ("AM_INSTANCE", "a1b2"), ("AM_DATA_DIR", "/data/iso")],
    ];
    const FORGED: [&str; 4] = ["--env", "AM_INSTANCE=forged", "--env=AM_DATA_DIR=/forged", "--no-focus"];

    /// 會建 pane 的指令（herdr 0.8.2 有 `--env` 的：pane split、tab create、workspace create；pane new 沿用同一條）：
    /// 呼叫者自帶的偽造值（兩種寫法）一律剝掉，只留母 pane 的值；母 pane 沒有（正式、遠端）就完全不帶（sol 五、六輪）。
    #[test]
    fn reserved_env_comes_only_from_the_parent_pane() {
        let s = Sandbox::new();
        let commands: [Vec<&str>; 4] = [
            vec!["pane", "split", "--pane", "w1:p1", "--direction", "right"],
            vec!["pane", "new", "--workspace", "w1"],
            vec!["tab", "create", "--workspace", "w1"],
            vec!["workspace", "create", "--cwd", "/tmp"],
        ];
        for parent in PARENTS {
            let iso = parent.iter().any(|(k, _)| *k == "AM_INSTANCE");
            for cmd in &commands {
                let mut args = cmd.clone();
                args.extend(FORGED);
                args.extend(["--env", "FOO=kept"]);
                let (out, err) = s.run(parent, &args);
                let case = format!("iso={iso} cmd={cmd:?} out={out:?} err={err}");
                let want = |v: &str| if iso { vec![v.to_string()] } else { vec![] };
                assert_eq!(env_values(&out, "AM_INSTANCE"), want("a1b2"), "{case}");
                assert_eq!(env_values(&out, "AM_DATA_DIR"), want("/data/iso"), "{case}");
                assert!(!out.iter().any(|a| a.contains("forged")), "{case}");
                assert_eq!(env_values(&out, "FOO"), vec!["kept".to_string()], "其他 --env 照舊：{case}");
                assert_eq!(&out[..cmd.len()], cmd.as_slice(), "子命令與原參數順序不變：{case}");
            }
        }
    }

    /// `herdr agent start` 沒有 `--env`（0.8.2：只有 NAME、--kind、--pane、--timeout、`--` 之後的 agent 參數）。
    /// 母 pane 有值也不能補，否則每一次開 child 都是未知旗標（sol 六輪）；呼叫者自帶的保留值照樣剝掉。
    #[test]
    fn agent_start_never_gains_an_env_flag() {
        let s = Sandbox::new();
        for parent in PARENTS {
            let mut args = vec!["agent", "start", "review", "--kind", "claude", "--pane", "w1:p1"];
            args.extend(&FORGED[..3]);
            args.extend(["--", "--env", "AM_INSTANCE=agent-cli-own"]);
            let (out, err) = s.run(parent, &args);
            let case = format!("parent={parent:?} out={out:?} err={err}");
            let head = out.iter().position(|a| a == "--").unwrap();
            assert!(!out[..head].iter().any(|a| a == "--env" || a.starts_with("--env=")), "`--` 之前不能有 --env：{case}");
            assert_eq!(&out[head..], ["--", "--env", "AM_INSTANCE=agent-cli-own"], "agent CLI 自己的參數原樣：{case}");
        }
    }

    /// issue #57：`agent start` 沒有 `--env`，全靠假設「`--pane` 指到的是 `pane split` 剛開的、帳號早
    /// 注入了」——這個假設一旦不成立（漏了 pane split、重用一顆沒走過那條路的舊 pane），子 agent 就
    /// 默默吃到預設帳號的額度。`exec` 真的 `agent start` 前，先對那個 `--pane` 補一行 export，不再只靠假設。
    #[test]
    fn agent_start_reexports_the_parents_account_env_into_the_target_pane_first() {
        let s = Sandbox::new();
        let log = s.dir.join("sendtext.log");
        let env = [
            ("AM_AGENT_NAME", "p-1"),
            ("AM_BOT_ID", "b1"),
            ("AM_HOOK_TOKEN", "tok"),
            ("AM_PORT", "7788"),
            ("CLAUDE_CONFIG_DIR", "/home/u/.claude-cc2"),
            ("AM_INSTANCE", "a1b2"),
            ("AM_DATA_DIR", "/data/iso"),
            ("AM_TEST_SENDTEXT_LOG", log.to_str().unwrap()),
            ("TMPDIR", s.dir.to_str().unwrap()),
        ];
        let (out, _) = s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--kind", "claude", "--pane", "w1:p9"], "agent start 的 argv 不變（herdr 沒有 --env）");
        let (pane, text) = s.sourced_env();
        assert_eq!(pane, "w1:p9", "補的是 --pane 指到的目標");
        assert!(text.contains("export CLAUDE_CONFIG_DIR='/home/u/.claude-cc2'"), "{text}");
        assert!(text.contains("export AM_BOT_ID='b1'"), "{text}");
        assert!(text.contains("export AM_HOOK_TOKEN='tok'"), "{text}");
        assert!(text.contains("export AM_INSTANCE='a1b2'"), "隔離實例的保留變數也補：{text}");
        assert!(text.contains("export AM_DATA_DIR='/data/iso'"), "{text}");
        assert!(text.contains("export AM_CHILD_OF='p-1'"), "子 agent 的 pane 要標出 parent：{text}");
    }

    /// §6.5i：子 agent 的指示跟母 bot 同一份（`AM_INSTRUCTIONS_FILE`），CLI 自己的指示檔不讀；呼叫者自己帶了就尊重。
    #[test]
    fn a_child_agent_gets_the_parents_instructions_file() {
        let s = Sandbox::new();
        let f = s.dir.join("instructions.md");
        // 真的指示檔是多行、可能有 tab：整份當一個參數，herdr ≥0.9.0 回 invalid_agent_argument（#772，假 herdr 照樣擋）。
        let body = "RULES say \"hi\" \\ done\n\n\t- second line 中文";
        std::fs::write(&f, format!("{body}\n")).unwrap();
        let fp = f.to_str().unwrap();
        let codex_home = s.dir.join("codex-home");
        let ch = codex_home.to_str().unwrap();
        let env = [("AM_AGENT_NAME", "p-1"), ("AM_BOT_ID", "b1"), ("AM_INSTRUCTIONS_FILE", fp), ("CODEX_HOME", ch)];
        let every_arg_fits_herdr = |out: &[String], err: &str| {
            assert!(!out.is_empty() && !err.contains("invalid_agent_argument"), "{out:?} {err}");
            assert!(out.iter().all(|a| !a.chars().any(char::is_control) && a.len() < 300), "{out:?}");
        };
        // claude：只傳路徑。
        let (out, err) = s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9"]);
        every_arg_fits_herdr(&out, &err);
        assert_eq!(&out[..10], ["agent", "start", "p-1-kid", "--kind", "claude", "--pane", "w1:p9", "--", "--append-system-prompt-file", fp]);
        // codex：沒有讀檔參數，指示寫進 `$CODEX_HOME/<profile>.config.toml`，argv 只帶 `-p <profile>`。
        let (out, err) = s.run(&env, &["agent", "start", "kid", "--kind", "codex", "--pane", "w1:p9", "--", "fork", "--last", "go"]);
        every_arg_fits_herdr(&out, &err);
        assert_eq!(
            &out[..15],
            ["agent", "start", "p-1-kid", "--kind", "codex", "--pane", "w1:p9", "--", "fork", "--last", "go", "-p", "am-child-b1", "-c", "project_doc_max_bytes=0"]
        );
        let profile = std::fs::read_to_string(codex_home.join("am-child-b1.config.toml")).unwrap();
        assert_eq!(profile, format!("developer_instructions = '''{body}'''\n"));
        // 呼叫者自己帶了 profile：不再補第二個 `-p`（clap 會報重覆），提醒指示檔沒帶到。
        let (out, err) = s.run(&env, &["agent", "start", "kid", "--kind", "codex", "--pane", "w1:p9", "--", "--profile", "mine"]);
        assert_eq!(out.iter().filter(|a| *a == "-p" || *a == "--profile").count(), 1, "{out:?}");
        assert!(err.contains("沒帶指示檔"), "{err}");
        // grok：沒有讀檔參數，`--rules` 給一行指向檔案的指示。
        let (out, err) = s.run(&env, &["agent", "start", "kid", "--kind", "grok", "--pane", "w1:p9"]);
        every_arg_fits_herdr(&out, &err);
        let rules = out.iter().position(|a| a == "--rules").expect("--rules");
        assert!(out[rules + 1].contains(fp), "{out:?}");
        // 自己帶了指示就不補；codex 沒有檔也照樣不讀 AGENTS.md。
        let (out, _) = s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9", "--", "--append-system-prompt", "mine"]);
        assert_eq!(out.iter().filter(|a| a.starts_with("--append-system-prompt")).count(), 1, "{out:?}");
        let gone = s.dir.join("gone.md");
        let (out, _) = s.run(&[("AM_AGENT_NAME", "p-1"), ("AM_INSTRUCTIONS_FILE", gone.to_str().unwrap()), ("CODEX_HOME", ch)], &["agent", "start", "kid", "--kind", "codex", "--pane", "w1:p9"]);
        assert_eq!(&out[out.len() - 3..], ["--", "-c", "project_doc_max_bytes=0"]);
        // 內容有三個連續單引號：codex 不帶（TOML 表示不了），也不會因此失敗。
        std::fs::write(&f, "a '''b'''").unwrap();
        let (out, err) = s.run(&env, &["agent", "start", "kid", "--kind", "codex", "--pane", "w1:p9"]);
        assert!(!out.iter().any(|a| a == "-p"), "{out:?}");
        assert!(err.contains("TOML 表示不了"), "{err}");
    }

    /// 假 herdr 跟 0.9.x 一樣擋控制字元：呼叫者自己塞多行參數會失敗，上面「herdr 收得下」的檢查才有意義（#772）。
    #[test]
    fn the_fake_herdr_rejects_control_characters_like_herdr_0_9() {
        let s = Sandbox::new();
        for bad in ["a\nb", "a\tb", "a\rb", "a\u{1b}b", "a\u{7f}b"] {
            let (out, err, rc) = s.run_full(&[("AM_AGENT_NAME", "p-1")], &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9", "--", bad]);
            assert_eq!(rc, 1, "{bad:?} {out:?}");
            assert!(err.contains("invalid_agent_argument"), "{bad:?} {err}");
        }
        let (out, _, rc) = s.run_full(&[("AM_AGENT_NAME", "p-1")], &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9", "--", "中文 'q' \"dq\" $HOME"]);
        assert_eq!(rc, 0, "{out:?}");
    }

    /// `agent start --help` 只是查用法：原樣轉給 herdr，不能把 env 補送打進呼叫者自己的 pane（2026-10-01 實際發生）。
    #[test]
    fn agent_start_help_is_passed_through_without_touching_any_pane() {
        let s = Sandbox::new();
        let log = s.dir.join("sendtext.log");
        let env = [("AM_AGENT_NAME", "p-1"), ("HERDR_PANE_ID", "w1:p1"), ("AM_TEST_SENDTEXT_LOG", log.to_str().unwrap()), ("TMPDIR", s.dir.to_str().unwrap())];
        let (out, _) = s.run(&env, &["agent", "start", "--help"]);
        assert_eq!(out, ["agent", "start", "--help"]);
        assert!(!log.exists(), "沒有任何 send-text：{:?}", std::fs::read_to_string(&log));
    }

    /// 使用者 2026-09-30：bot 開的 pane 標 `AM_CHILD_OF=<母名>`（子代再開的 pane 沿用同一個值、呼叫者偽造的剝掉），
    /// 有這個標記的 pane 開 `agent start` 一律拒絕，而且 herdr 根本沒被叫到。
    #[test]
    fn a_child_pane_is_marked_and_cannot_start_a_grandchild() {
        let s = Sandbox::new();
        let parent = [("AM_AGENT_NAME", "p-1"), ("AM_BOT_ID", "b1")];
        let (out, _) = s.run(&parent, &["pane", "split", "--pane", "w1:p1", "--env", "AM_CHILD_OF=forged"]);
        assert_eq!(env_values(&out, "AM_CHILD_OF"), vec!["p-1".to_string()], "{out:?}");
        let (out, _) = s.run(&[("AM_AGENT_NAME", "p-1")], &["pane", "split", "--pane", "w1:p1"]);
        assert!(env_values(&out, "AM_CHILD_OF").is_empty(), "沒有 bot 身分的人工 shell 不標：{out:?}");

        let child = [("AM_AGENT_NAME", "p-1"), ("AM_BOT_ID", "b1"), ("AM_CHILD_OF", "p-1")];
        let (out, _) = s.run(&child, &["pane", "split", "--pane", "w1:p2"]);
        assert_eq!(env_values(&out, "AM_CHILD_OF"), vec!["p-1".to_string()], "子代的 pane 沿用同一個 parent：{out:?}");
        let (out, err, rc) = s.run_full(&child, &["agent", "start", "grandkid", "--pane", "w1:p2"]);
        assert_eq!(rc, 77, "{out:?} {err}");
        assert!(out.is_empty(), "herdr 不能被叫到：{out:?}");
        assert!(err.contains("禁止再開子 agent") && err.contains("`p-1` 決定"), "{err}");
    }

    /// 值裡有單引號要逃脫，不然那個 export 的邊界會斷在半路。
    #[test]
    fn agent_start_reexport_escapes_single_quotes_in_values() {
        let s = Sandbox::new();
        let log = s.dir.join("sendtext.log");
        let env = [("AM_AGENT_NAME", "p-1"), ("AM_OUTBOX", "/data/o'tbox"), ("AM_TEST_SENDTEXT_LOG", log.to_str().unwrap()), ("TMPDIR", s.dir.to_str().unwrap())];
        s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9"]);
        let (_, sent) = s.sourced_env();
        assert!(sent.contains(r"export AM_OUTBOX='/data/o'\''tbox'"), "{sent}");
        // 暫存檔路徑本身含單引號也不會斷在半路。
        let quoted = s.dir.join("q'dir");
        std::fs::create_dir_all(&quoted).unwrap();
        std::fs::remove_file(&log).unwrap();
        let env = [("AM_AGENT_NAME", "p-1"), ("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "7788"), ("AM_TEST_SENDTEXT_LOG", log.to_str().unwrap()), ("TMPDIR", quoted.to_str().unwrap())];
        s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9"]);
        let (_, sent) = s.sent_text();
        assert!(sent.starts_with(" . '") && sent.contains(r"q'\''dir"), "{sent}");
    }

    /// #389：PATH 去重（保留第一次出現的順序），而且 send-text 沒有整串 export 的痕跡；其他變數照舊完整。
    #[test]
    fn agent_start_env_file_dedupes_path_and_keeps_every_other_variable() {
        let s = Sandbox::new();
        let log = s.dir.join("sendtext.log");
        let base = format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display());
        let fat = format!("{base}:/opt/vercel/0.49.1:/opt/ts:/opt/vercel/0.49.1:{base}:/opt/vercel/0.49.2:/opt/ts");
        let env = [
            ("PATH", fat.as_str()),
            ("AM_AGENT_NAME", "p-1"),
            ("AM_BOT_ID", "b1"),
            ("AM_HOOK_TOKEN", "tok"),
            ("AM_PORT", "7788"),
            ("CODEX_HOME", "/home/u/.codex"),
            ("CLAUDE_CONFIG_DIR", "/home/u/.claude-cc2"),
            ("AM_TEST_SENDTEXT_LOG", log.to_str().unwrap()),
            ("TMPDIR", s.dir.to_str().unwrap()),
        ];
        s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9"]);
        let (_, body) = s.sourced_env();
        let want = format!("{base}:/opt/vercel/0.49.1:/opt/ts:/opt/vercel/0.49.2");
        assert!(body.contains(&format!("export PATH='{want}'\n")), "{body}");
        for kv in ["AM_BOT_ID='b1'", "CODEX_HOME='/home/u/.codex'", "CLAUDE_CONFIG_DIR='/home/u/.claude-cc2'", "AM_AGENT_NAME='p-1'"] {
            assert!(body.contains(&format!("export {kv}\n")), "{kv} 要完整帶到：{body}");
        }
        let (_, on_screen) = s.sent_text();
        assert!(!on_screen.contains("export") && !on_screen.contains("/opt/"), "畫面上只有 source 一行：{on_screen}");
    }

    /// 暫存檔不准放 scratchpad／outbox：`$TMPDIR` 指到那裡就改用 /tmp。
    #[test]
    fn the_env_file_never_lands_in_a_scratchpad_or_outbox() {
        for bad in ["scratchpad", "outbox/bot1"] {
            let s = Sandbox::new();
            let log = s.dir.join("sendtext.log");
            let dir = s.dir.join(bad);
            std::fs::create_dir_all(&dir).unwrap();
            let env = [("AM_AGENT_NAME", "p-1"), ("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "7788"), ("AM_TEST_SENDTEXT_LOG", log.to_str().unwrap()), ("TMPDIR", dir.to_str().unwrap())];
            s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9"]);
            let (_, text) = s.sent_text();
            assert!(text.starts_with(" . '/tmp/am-env."), "{bad}：{text}");
            let file = text.trim().strip_prefix(". '").unwrap().split('\'').next().unwrap().to_string();
            let _ = std::fs::remove_file(file);
        }
    }

    /// 暫存檔寫不出來（目錄不存在）：退回逐行 export，環境不能丟。
    #[test]
    fn an_unwritable_env_file_falls_back_to_inline_exports() {
        let s = Sandbox::new();
        let log = s.dir.join("sendtext.log");
        let env = [("AM_AGENT_NAME", "p-1"), ("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "7788"), ("AM_TEST_SENDTEXT_LOG", log.to_str().unwrap()), ("TMPDIR", "/nonexistent-am-dir")];
        s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9"]);
        let text = s.sent_text().1;
        assert!(text.contains("export AM_BOT_ID='b1'; "), "{text}");
    }

    /// `--pane` 沒給（herdr 自己會因為缺必要旗標報錯）：shim 沒有目標可補，不猜、不炸。
    #[test]
    fn agent_start_without_a_pane_does_not_reexport_anything() {
        let s = Sandbox::new();
        let log = s.dir.join("sendtext.log");
        let env = [("AM_AGENT_NAME", "p-1"), ("CLAUDE_CONFIG_DIR", "/home/u/.claude-cc2"), ("AM_TEST_SENDTEXT_LOG", log.to_str().unwrap())];
        let (out, _) = s.run(&env, &["agent", "start", "kid", "--kind", "claude"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--kind", "claude"]);
        assert!(!log.exists() || std::fs::read_to_string(&log).unwrap().trim().is_empty(), "沒有目標 pane，不猜著補");
    }

    /// 2026-10-01：`pane split` 失敗、`--pane ""` 傳下來時，shim 退回 $HERDR_PANE_ID（呼叫者自己的 pane），
    /// 把補送 env 那一行打進了使用者的輸入框。空的 `--pane`、沒帶、指到自己，都不能碰自己的 pane。
    #[test]
    fn agent_start_never_types_into_the_callers_own_pane() {
        let s = Sandbox::new();
        let log = s.dir.join("sendtext.log");
        let base = [("AM_AGENT_NAME", "p-1"), ("HERDR_PANE_ID", "w1:p1"), ("AM_TEST_SENDTEXT_LOG", log.to_str().unwrap()), ("TMPDIR", s.dir.to_str().unwrap())];
        // 人工 shell（沒有 bot 身分）：照舊轉給 herdr（它自己會因為缺 --pane 報錯），但不補送。
        let (out, _) = s.run(&base, &["agent", "start", "kid", "--kind", "claude", "--pane", ""]);
        assert_eq!(&out[..3], ["agent", "start", "p-1-kid"]);
        // 受管的 bot：沒帶／空的 --pane 直接拒絕，herdr 也不叫。
        let managed: Vec<(&str, &str)> = base.iter().copied().chain([("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1")]).collect();
        for argv in [&["agent", "start", "kid", "--kind", "claude", "--pane", ""][..], &["agent", "start", "kid", "--kind", "claude"][..]] {
            let (out, err, rc) = s.run_full(&managed, argv);
            assert_eq!(rc, 2, "{argv:?} {err}");
            assert!(out.is_empty() && err.contains("pane split"), "{argv:?} {out:?} {err}");
        }
        // 指到自己的 pane：誰都不行。
        let (out, err, rc) = s.run_full(&base, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p1"]);
        assert_eq!(rc, 2, "{err}");
        assert!(out.is_empty(), "{out:?}");
        assert!(!log.exists() || std::fs::read_to_string(&log).unwrap().trim().is_empty(), "自己的 pane 一個字都不能收到");
    }

    /// 真的 herdr 在的話，把 shim 產生的 argv 丟給它的 parser：在最後（`--` 之前）放一個假旗標，
    /// 回報的未知選項是那個假旗標，就代表前面每個參數它都認得。找不到真的 herdr 就略過。
    #[test]
    fn the_generated_argv_parses_with_the_real_herdr() {
        let Some(real) = real_herdr() else {
            eprintln!("skip: no real herdr on PATH");
            return;
        };
        let s = Sandbox::new();
        let cases: [Vec<&str>; 4] = [
            vec!["agent", "start", "review", "--kind", "claude", "--pane", "w1:p1", "--env", "AM_INSTANCE=forged"],
            vec!["pane", "split", "--pane", "w1:p1", "--direction", "right", "--env", "AM_DATA_DIR=/forged"],
            vec!["tab", "create", "--workspace", "w1", "--env=AM_INSTANCE=forged"],
            vec!["workspace", "create", "--cwd", "/tmp", "--env", "AM_INSTANCE=forged"],
        ];
        for parent in PARENTS {
            for args in &cases {
                let (mut out, _) = s.run(parent, args);
                let at = out.iter().position(|a| a == "--").unwrap_or(out.len());
                out.insert(at, "--am-dry-parse".into());
                let res = std::process::Command::new(&real).args(&out).output().unwrap();
                let text = format!("{}{}", String::from_utf8_lossy(&res.stdout), String::from_utf8_lossy(&res.stderr));
                assert!(
                    text.contains("unknown option: --am-dry-parse") || text.contains("unexpected argument '--am-dry-parse'"),
                    "真的 herdr 不認得 shim 產生的參數：parent={parent:?} argv={out:?}\n{text}"
                );
            }
        }
    }

    /// PATH 上第一個不是 agents-manager shim 的 herdr（或 `AM_REAL_HERDR`）。
    fn real_herdr() -> Option<std::path::PathBuf> {
        if let Some(p) = std::env::var_os("AM_REAL_HERDR").map(std::path::PathBuf::from).filter(|p| p.is_file()) {
            return Some(p);
        }
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("herdr"))
            .find(|p| p.is_file() && !std::fs::read(p).map(|b| b.starts_with(b"#!")).unwrap_or(true))
    }

    #[test]
    fn every_other_subcommand_is_forwarded_verbatim() {
        let s = Sandbox::new();
        let (out, err) = s.run(&[("AM_AGENT_NAME", "p-1")], &["agent", "list"]);
        assert_eq!(out, ["agent", "list"]);
        assert_eq!(err, "");
    }

    fn curl_log(s: &Sandbox) -> std::path::PathBuf {
        let log = s.dir.join("curl.log");
        s.install_fake_curl(&format!(
            "printf '%s\\n' \"$*\" >> '{}'\n\
             case \"$*\" in\n\
               */relay/spawn/begin*) printf '{{\"permit_id\":\"test-permit\"}}\\n200'; exit 0 ;;\n\
               */relay/spawn/abort*) printf '{{\"released\":true}}\\n200'; exit 0 ;;\n\
               */relay/spawn/finish*) printf '{{\"registered\":true}}\\n200'; exit 0 ;;\n\
             esac\n",
            log.display()
        ));
        log
    }

    /// #664：agent start 失敗要 abort，不能把 permit 留到 daemon 重啟。
    #[test]
    fn a_failed_agent_start_releases_the_spawn_permit() {
        let s = Sandbox::new();
        let log = curl_log(&s);
        let env = [
            ("AM_AGENT_NAME", "parent"),
            ("AM_BOT_ID", "b1"),
            ("AM_HOOK_TOKEN", "tok"),
            ("AM_PORT", "7788"),
            ("HERDR_PANE_ID", "w1:p0"),
            ("AM_TEST_HERDR_RC", "3"),
            ("TMPDIR", s.dir.to_str().unwrap()),
        ];
        let (out, err, rc) = s.run_full(&env, &["agent", "start", "kid", "--pane", "w1:p1"]);
        assert_eq!(rc, 3, "{out:?} {err}");
        let sent = std::fs::read_to_string(&log).unwrap();
        assert!(sent.contains("/relay/spawn/abort"), "{sent}");
        assert!(!sent.contains("/relay/spawn/finish"), "失敗沒有新 pane，不能 finish：{sent}");
    }

    /// #664：herdr 還在跑時被中斷，trap 也要 abort。
    #[test]
    fn an_interrupted_agent_start_releases_the_spawn_permit() {
        let s = Sandbox::new();
        let log = curl_log(&s);
        let env = [
            ("AM_AGENT_NAME", "parent"),
            ("AM_BOT_ID", "b1"),
            ("AM_HOOK_TOKEN", "tok"),
            ("AM_PORT", "7788"),
            ("HERDR_PANE_ID", "w1:p0"),
            ("AM_TEST_SIGNAL_PARENT", "1"),
            ("TMPDIR", s.dir.to_str().unwrap()),
        ];
        let (out, err, rc) = s.run_full(&env, &["agent", "start", "kid", "--pane", "w1:p1"]);
        assert_eq!(rc, 130, "{out:?} {err}");
        let sent = std::fs::read_to_string(&log).unwrap();
        assert!(sent.contains("/relay/spawn/abort"), "{sent}");
    }

    /// 遠端主機上的 bot 沒有 `AM_PORT`（SPEC §11.4）：spawn fence 問不到 daemon 也不能擋，
    /// pane split 與 agent start 都要照開，而且一通 curl 都不打（127.0.0.1 在遠端是那台機器自己）。
    #[test]
    fn a_remote_bot_without_am_port_still_spawns_children() {
        let s = Sandbox::new();
        let log = curl_log(&s);
        let created = r#"{"id":"pane.created","result":{"pane":{"pane_id":"w1:p-child"}}}"#;
        let env = [
            ("AM_AGENT_NAME", "parent"),
            ("AM_BOT_ID", "b1"),
            ("AM_BOT_TOKEN", "tok"),
            ("HERDR_PANE_ID", "w1:p0"),
            ("AM_TEST_CREATE_JSON", created),
            ("TMPDIR", s.dir.to_str().unwrap()),
        ];
        let (out, err, rc) = s.run_full(&env, &["pane", "split", "--pane", "w1:p1"]);
        assert_eq!(rc, 0, "{out:?} {err}");
        assert!(!out.is_empty(), "herdr pane split 要真的跑：{err}");
        let (out, err, rc) = s.run_full(&env, &["agent", "start", "kid", "--pane", "w1:p1"]);
        assert_eq!(rc, 0, "{out:?} {err}");
        assert!(!err.contains("拒絕"), "{err}");
        assert!(!log.exists(), "遠端 bot 不該打任何 curl：{:?}", std::fs::read_to_string(&log));
    }

    /// #665：母名已經 32 字時，子 agent 不能被改成母 bot 自己；不同字尾也不能撞成同一個名字。
    #[test]
    fn a_child_of_a_max_length_parent_is_not_renamed_to_the_parent() {
        let s = Sandbox::new();
        let parent = "paaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(parent.len(), 32, "{parent}");
        let env = [("AM_AGENT_NAME", parent), ("HERDR_PANE_ID", "w1:p1")];
        let mut names = Vec::new();
        for suffix in ["review", "reviewer2"] {
            let (out, err, rc) = s.run_full(&env, &["agent", "start", suffix]);
            assert_eq!(rc, 0, "{suffix}: {out:?} {err}");
            let name = out.iter().find(|a| *a != "agent" && *a != "start").cloned().unwrap();
            assert_ne!(name, parent, "{err}");
            assert!(name.len() <= 32 && name.contains('-'), "{name}");
            assert!(err.contains(&name), "{err}");
            names.push(name);
        }
        assert_ne!(names[0], names[1], "兩個字尾不能截成同一個名字：{names:?}");

        let (out, err, rc) = s.run_full(&env, &["agent", "prompt", "review", "please check"]);
        assert_eq!(rc, 0, "{out:?} {err}");
        assert_eq!(out.first().map(String::as_str), Some("agent"));
        assert_eq!(out.get(1).map(String::as_str), Some("prompt"));
        assert_ne!(out.get(2).map(String::as_str), Some(parent), "prompt 不能打回母 bot：{out:?}");
        assert_eq!(out.get(2).map(String::as_str), Some(names[0].as_str()), "prompt 用的名字要跟 start 一樣：{out:?}");
    }
}
