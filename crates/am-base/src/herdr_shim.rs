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

# Its herdr `--env` arguments carry bot tokens, so shell xtrace must not copy them into logs.
set +x

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
AM_RESERVED_ENV_KEYS="CLAUDE_CONFIG_DIR CODEX_HOME AM_BOT_ID AM_BOT_TOKEN AM_HOOK_TOKEN AM_PORT AM_RUN_ID AM_AGENT_NAME AM_KIND AM_MODEL AM_EFFORT AM_PROJECT_ID AM_WORKSPACE_ID AM_OUTBOX AM_DAEMON_EXE AM_CONFIG_PATH AM_REAL_HERDR PATH CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION CLAUDE_CODE_DISABLE_CLAUDE_MDS AM_INSTRUCTIONS_FILE AM_KEEP_CLI_DOCS"

# API calls use the independent bot credential. Keep the hook-token fallback for already-running
# hook-enabled panes until they restart to receive AM_BOT_TOKEN.
am_bot_token() {
    if [ -n "${AM_BOT_TOKEN:-}" ]; then printf '%s' "$AM_BOT_TOKEN"; else printf '%s' "${AM_HOOK_TOKEN:-}"; fi
}

# Keep bot credentials out of curl's process arguments; curl reads this header from stdin.
am_curl_bot() {
    _tok=$(am_bot_token)
    [ -n "$_tok" ] || return 77
    printf 'X-AM-Bot-Token: %s\n' "$_tok" | curl -H @- "$@"
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

# 注入的參數值不能含控制字元：herdr ≥0.9 只要有一個參數壞掉就拒絕整個 `agent start`（invalid_agent_argument，#772），
# 子 agent 就開不起來。寧可不帶那個參數、說一聲。
am_arg_ok() {
    [ "$(printf '%s' "$1" | LC_ALL=C tr -d '\000-\037\177')" = "$1" ] || return 1
    # Rust char::is_control also rejects UTF-8 C1 controls (U+0080..U+009F), which are the
    # byte pairs C2 80..C2 9F. Match those bytes in the C locale without rejecting other UTF-8.
    _am_c1_pattern=$(printf '\302[\200-\237]')
    if printf '%s' "$1" | LC_ALL=C grep -q "$_am_c1_pattern"; then return 1; fi
    return 0
}

# 檔案內容 → TOML 多行字面字串（三個單引號包起來），給 codex 的 `developer_instructions`（§6.5i）。
# 字面字串不跳脫任何字元，碰不到各家 awk／sed 的反斜線差異；內容本身有三個連續單引號就表示不了，回 1 讓呼叫端不帶。
# TOML 多行字面字串不收控制字元（只有 tab 與換行可以；真 codex 讀到壞掉的 profile 整個起不來），所以先濾掉再包。
am_toml_string_of_file() {
    _q3="'''"
    _am_c1_pattern=$(printf '\302[\200-\237]')
    _body=$(LC_ALL=C tr -d '\000-\010\013-\037\177' < "$1" 2>/dev/null) || return 1
    _body=$(printf '%s' "$_body" | LC_ALL=C sed "s/${_am_c1_pattern}//g") || return 1
    case "$_body" in
        *"$_q3"*) return 1 ;;
    esac
    printf '%s%s%s' "$_q3" "$_body" "$_q3"
}

# codex 沒有「從檔案讀 developer_instructions」的參數；`-c developer_instructions=<內容>` 是多行，herdr ≥0.9.0
# 的 `agent start` 擋所有控制字元（invalid_agent_argument，#772），壓成一行又會撞到打字長度上限（herdr.rs 的
# `fit_command_line`）。改寫成 `$CODEX_HOME/<名>.config.toml`，argv 只帶 `-p <名>`；印出 profile 名，失敗回 1。
# 每顆 bot 一個檔（同帳號的 bot 共用 CODEX_HOME），0600（內容是 AG Man 的規則）、暫存檔＋mv 換上，兩個 child 同時開也不會讀到半份。
# 回傳碼：0＝成功（印 profile 名）、2＝寫不進去、3＝內容表示不了（三個連續單引號）。
am_codex_instructions_profile() {
    _toml=$(am_toml_string_of_file "$1") || return 3
    _home=${CODEX_HOME:-$HOME/.codex}
    _name="am-child-$(printf '%s' "${AM_BOT_ID:-${AM_AGENT_NAME:-local}}" | tr -c 'A-Za-z0-9_-' '_')"
    _dst="$_home/$_name.config.toml"
    mkdir -p "$_home" 2>/dev/null
    # A predictable PID suffix may collide with a stale file or symlink; mktemp creates a fresh
    # same-directory inode, and chmod closes the permission gap before the atomic rename.
    _tmp=$(umask 077; mktemp "${_dst}.XXXXXX" 2>/dev/null) || _tmp=""
    if [ -n "$_tmp" ] && ( umask 077; printf 'developer_instructions = %s\n' "$_toml" > "$_tmp" ) 2>/dev/null \
        && chmod 600 "$_tmp" 2>/dev/null && mv -f "$_tmp" "$_dst" 2>/dev/null; then
        am_sweep_codex_profiles "$_home"
        printf '%s' "$_name"
        return 0
    fi
    [ -z "$_tmp" ] || rm -f "$_tmp"
    return 2
}

# 殘留清理：寫到一半被殺掉的暫存檔（超過 10 分鐘）、超過 30 天沒重寫的 `am-child-*` profile。profile 每次開 child 都重寫，
# 所以掃掉舊的不會害到誰；只認 `am-child-*.config.toml*`，CODEX_HOME 裡別的檔案（config.toml、自己的 profile）不碰。
am_sweep_codex_profiles() {
    # find rounds ages down to whole minutes/days; +9 and +29 implement the 10-minute / 30-day cutoffs.
    find "$1" -maxdepth 1 \( -name 'am-child-*.config.toml.*' -mmin +9 -o -name 'am-child-*.config.toml' -mtime +29 \) -exec rm -f {} + 2>/dev/null
    return 0
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
    _out=$(am_curl_bot -s -m 2 -w '\n%{http_code}' -X POST "http://127.0.0.1:${AM_PORT}/relay/spawn/begin" \
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
    _out=$(am_curl_bot -s -m 2 -w '\n%{http_code}' -X POST "http://127.0.0.1:${AM_PORT}/relay/spawn/finish" \
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
        am_curl_bot -s -m 2 -X POST "http://127.0.0.1:${AM_PORT}/relay/spawn/abort" \
            --data-urlencode "bot_id=${AM_BOT_ID}" \
            --data-urlencode "permit_id=${_AM_SPAWN_PERMIT}" >/dev/null 2>&1 || true
    fi
    _AM_SPAWN_PERMIT=""
    return 0
}

# The positional name after `herdr agent start` becomes `<AM_AGENT_NAME>-<name>`, unless it
# already carries the prefix. herdr agent names
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
    # `--help` 只是查用法：不開 pane、不補 env。以前照樣走 spawn 流程，env 補送就打進呼叫者自己的 pane（目標預設是 ${HERDR_PANE_ID}）。
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
        if am_arg_ok "$AM_MODEL"; then
            [ "$_stop" = 1 ] || set -- "$@" --
            _stop=1
            set -- "$@" --model "$AM_MODEL"
            if [ -n "${AM_EFFORT:-}" ] && [ "$_has_effort" = 0 ] && [ "${AM_KIND:-}" = "claude" ]; then
                if am_arg_ok "$AM_EFFORT"; then
                    set -- "$@" --effort "$AM_EFFORT"
                else
                    printf 'agents-manager: AM_EFFORT 含控制字元，子 agent 這次沒帶 --effort（herdr 會拒絕整個 agent start）\n' >&2
                fi
            fi
            printf 'agents-manager: 子 agent 沒指定模型，沿用母 bot 的 `%s`\n' "$AM_MODEL" >&2
        else
            printf 'agents-manager: AM_MODEL 含控制字元，子 agent 這次沒帶 --model（herdr 會拒絕整個 agent start）\n' >&2
        fi
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
                if am_arg_ok "$_instr"; then
                    [ "$_stop" = 1 ] || set -- "$@" --
                    _stop=1
                    set -- "$@" --append-system-prompt-file "$_instr"
                else
                    printf 'agents-manager: 指示檔路徑含控制字元，子 agent 這次沒帶指示檔（herdr 會拒絕整個 agent start）\n' >&2
                fi
            fi
            ;;
        codex)
            if [ -n "$_instr" ]; then
                [ "$_stop" = 1 ] || set -- "$@" --
                _stop=1
                if [ "$_has_profile" = 1 ]; then
                    printf 'agents-manager: 你自己帶了 codex 的 -p/--profile，子 agent 這次沒帶指示檔 %s\n' "$_instr" >&2
                else
                    _prof=$(am_codex_instructions_profile "$_instr")
                    case $? in
                        0) set -- "$@" -p "$_prof" ;;
                        3) printf 'agents-manager: %s 含有三個連續單引號，TOML 表示不了，子 agent 這次沒帶指示檔\n' "$_instr" >&2 ;;
                        *) printf 'agents-manager: 寫不進 codex 的 profile（%s），子 agent 這次沒帶指示檔\n' "${CODEX_HOME:-$HOME/.codex}" >&2 ;;
                    esac
                fi
            fi
            # 只在 §6.5i 底下（daemon 給了 AM_INSTRUCTIONS_FILE）才關：人工 shell 的 codex 照它自己的習慣。
            # `AM_KEEP_CLI_DOCS`：母 bot 啟動時有一份 agent md 讀不到（#769），這一輪不關，子 agent 才讀得到 AGENTS.md。
            if [ "$_has_docs" = 0 ] && [ -n "${AM_INSTRUCTIONS_FILE:-}" ] && [ -z "${AM_KEEP_CLI_DOCS:-}" ]; then
                [ "$_stop" = 1 ] || set -- "$@" --
                _stop=1
                set -- "$@" -c project_doc_max_bytes=0
            fi
            ;;
        grok)
            if [ -n "$_instr" ]; then
                if am_arg_ok "$_instr"; then
                    [ "$_stop" = 1 ] || set -- "$@" --
                    _stop=1
                    # grok 的 `--rules` 只收字串、沒有讀檔版：給一行指向檔案的指示，讓它開工前自己讀。
                    # 路徑用反引號框起來：有空白時才分得出哪裡到哪裡。
                    set -- "$@" --rules "AG Man 指示（硬規則，效力同系統指示）在 \`${_instr}\`：開始任何工作前先完整讀過並照做。"
                else
                    printf 'agents-manager: 指示檔路徑含控制字元，子 agent 這次沒帶指示檔（herdr 會拒絕整個 agent start）\n' >&2
                fi
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
    # 目標 pane 裡已經有 agent（別顆 bot 的 pane、拿錯的 id）就一個字都不送：補送 env 那行會被打進那顆 agent 的
    # 輸入框（2026-10-02 A issuers-1 拿 wW:p3 去開 child，那是別顆 bot 的 pane，使用者收到一行 ` . '/tmp/am-env…'`）。
    # 問不到（pane 不在、herdr 沒回）就照舊交給 herdr 自己判斷，只有確定有 agent 才擋。
    _tagent=$("$AM_HERDR" pane get "$_pane" 2>/dev/null | tr ',' '\n' | sed -n 's/.*"agent" *: *"\([^"]*\)".*/\1/p' | head -n 1)
    if [ -n "$_tagent" ]; then
        printf 'agents-manager: --pane %s 裡已經在跑 %s，不是空的 shell；先 herdr pane split 開新 pane，再把新 pane id 給 --pane。沒有送任何東西\n' "$_pane" "$_tagent" >&2
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
    # 暫存檔放 ${TMPDIR}（不能是 scratchpad／outbox）或 /tmp；寫不出來才退回舊的逐行 export，環境不能丟。
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
            _out=$(am_curl_bot -s -m "$_m" -w '\n%{http_code}' -X POST "http://127.0.0.1:${AM_PORT}/relay/announce" \
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
    # 先問 herdr 母 pane（${HERDR_PANE_ID}）**現在**在哪個 workspace，問不到才用 AM_WORKSPACE_ID：daemon 在決定 workspace
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
        am_curl_bot -s -m 2 -X POST "http://127.0.0.1:${AM_PORT}/relay/pane" \
            --data-urlencode "bot_id=${AM_BOT_ID}" \
            --data-urlencode "pane_id=${_pane}" \
            --data-urlencode "purpose=${_purpose}" >/dev/null 2>&1 || true
        exit 0
    fi
    exec "$AM_HERDR" "$_sub1" "$_sub2" "$@"
}

# `agent list` 之後把子 agent 的 prompt cache 還熱多久印到 stderr（使用者 2026-10-04：派工優先用 cache 還熱的 child，
# 省 token）。herdr 的 JSON 原封不動在 stdout、exit code 照舊；問不到 daemon、或自己就是子 agent（不能派工），就不印。
am_agent_list() {
    "$AM_HERDR" "$@"
    _rc=$?
    if [ -z "${AM_CHILD_OF:-}" ] && [ -n "${AM_BOT_ID:-}" ] && [ -n "${AM_PORT:-}" ] && [ -n "$(am_bot_token)" ] \
        && command -v curl >/dev/null 2>&1; then
        _kids=$(am_curl_bot -s -f -m 2 -X POST "http://127.0.0.1:${AM_PORT}/relay/kids" \
            --data-urlencode "bot_id=${AM_BOT_ID}" 2>/dev/null) || _kids=""
        [ -z "$_kids" ] || printf '%s\n' "$_kids" >&2
    fi
    exit "$_rc"
}

AM_HERDR=$(am_real_herdr | head -n 1)
if [ -z "$AM_HERDR" ]; then
    printf 'agents-manager: 找不到真正的 herdr（把它的路徑放進 AM_REAL_HERDR）\n' >&2
    exit 127
fi

case "${1:-} ${2:-}" in
    "agent start") am_agent_start "$@" ;;
    "agent prompt") am_agent_prompt "$@" ;;
    "agent list") am_agent_list "$@" ;;
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
