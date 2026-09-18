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

# 帳號／hook 的保留清單：pane split／tab create／workspace create 補 `--env` 用（am_forward_with_env），
# agent start 沒有 `--env` 時補 export 用（am_reexport_env_before_start，issue #57）。單一清單，兩邊不會走鐘。
#
# `AM_DAEMON_EXE`／`AM_CONFIG_PATH`（issue #138）：cargo shim 把 check／test／clippy 轉到外部編譯主機的前提。
# 少了它們，子 agent 的 cargo 永遠留在本機——#104 當初只在 daemon 注入端加了，這份清單漏了。
# 跟其他 key 一樣：母 pane 有才帶、呼叫者自己給了就尊重（它們不是隔離實例那種要防偽造的保留變數）。
AM_RESERVED_ENV_KEYS="CLAUDE_CONFIG_DIR CODEX_HOME AM_BOT_ID AM_HOOK_TOKEN AM_PORT AM_RUN_ID AM_AGENT_NAME AM_KIND AM_MODEL AM_EFFORT AM_PROJECT_ID AM_WORKSPACE_ID AM_OUTBOX AM_DAEMON_EXE AM_CONFIG_PATH AM_REAL_HERDR PATH"

# `<name>` → `<AM_AGENT_NAME>-<name>`, unless it already carries the prefix. herdr agent names
# are `[a-z][a-z0-9_-]{0,31}`, so the result is cut to 32.
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
    _full=$(printf '%s-%s' "$AM_AGENT_NAME" "$_n" | cut -c1-32)
    printf 'agents-manager: 子 agent 已改名為 `%s`，才會掛在 `%s` 底下被追蹤\n' "$_full" "$AM_AGENT_NAME" >&2
    printf '%s' "$_full"
}

# `herdr agent start [flags] <name> …` — rewrite the first bare word after `start`, which is
# the agent name. Flags may come first, so skip options and the values of the ones that take
# one, and stop at `--` (everything after it is the agent's own argv).
am_agent_start() {
    shift 2
    _n=$#
    _i=0
    _named=0
    _stop=0
    _prev=""
    _kind=""
    _pane=""
    _has_model=0
    _has_effort=0
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
                            AM_INSTANCE=* | AM_DATA_DIR=*)
                                shift
                                _i=$((_i + 1))
                                continue
                                ;;
                        esac
                    fi
                    ;;
                --env=AM_INSTANCE=* | --env=AM_DATA_DIR=*) continue ;;
            esac
        fi
        if [ "$_prev" = "--kind" ]; then _kind=$_a; fi
        if [ "$_prev" = "--pane" ]; then _pane=$_a; fi
        if [ "$_stop" = 1 ]; then
            # codex / grok spell it `-m`, codex also `-c model=…`: all of them are "the child
            # picked its own model" and must not be overridden with the parent's.
            case "$_a" in
                --model | --model=* | -m | -m=* | model=*) _has_model=1 ;;
                --effort | --effort=*) _has_effort=1 ;;
                -c) : ;;
                model_reasoning_effort=*) _has_effort=1 ;;
            esac
        elif [ "$_named" = 0 ]; then
            case "$_prev" in
                --kind | --pane | --timeout) : ;;
                *)
                    case "$_a" in
                        --) _stop=1 ;;
                        -*) : ;;
                        *)
                            _a=$(am_child_name "$_a")
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
        set -- "$@" --model "$AM_MODEL"
        if [ -n "${AM_EFFORT:-}" ] && [ "$_has_effort" = 0 ] && [ "${AM_KIND:-}" = "claude" ]; then
            set -- "$@" --effort "$AM_EFFORT"
        fi
        printf 'agents-manager: 子 agent 沒指定模型，沿用母 bot 的 `%s`\n' "$AM_MODEL" >&2
    fi
    am_reexport_env_before_start "$_pane"
    exec "$AM_HERDR" agent start "$@"
}

# issue #57：`agent start` 沒有 `--env`，全靠假設「目標 pane 是 pane split 剛開的、帳號早就注入了」——
# 漏了那一步、或重用一顆沒走過那條路的舊 pane 時，子 agent 就默默吃到預設帳號（cc0）的額度。
# 補送一行 export 到 `--pane` 指到的目標，跟 agent.start 前補 PATH（`start_inner`）用同一招：
# pty 會緩衝這行輸入，pane 還沒起殻也不怕；pane 早有正確值時只是重覆設一次，無害。
am_reexport_env_before_start() {
    _pane=$1
    [ -n "$_pane" ] || return 0
    _line=""
    for _k in $AM_RESERVED_ENV_KEYS AM_INSTANCE AM_DATA_DIR; do
        eval "_v=\${$_k:-}"
        [ -n "$_v" ] || continue
        _esc=$(printf '%s' "$_v" | sed "s/'/'\\\\''/g")
        _line="${_line}export $_k='$_esc'; "
    done
    [ -n "$_line" ] || return 0
    "$AM_HERDR" pane send-text "$_pane" "$_line
" >/dev/null 2>&1
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
        _name=$(am_child_name "$_name")
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
    if [ -n "${AM_BOT_ID:-}" ] && [ -n "${AM_HOOK_TOKEN:-}" ] && [ -n "${AM_PORT:-}" ] && command -v curl >/dev/null 2>&1; then
        # 表單編碼：prompt 內容有引號、換行、`&` 都不會壞，也不必在 sh 裡拼 JSON。
        _resp=$(curl -s -m 2 -X POST "http://127.0.0.1:${AM_PORT}/relay/announce" \
            -H "X-AM-Bot-Token: ${AM_HOOK_TOKEN}" \
            --data-urlencode "bot_id=${AM_BOT_ID}" \
            --data-urlencode "to_agent=${_name}" \
            --data-urlencode "text=${_text}" \
            --data-urlencode "ack=${_ack}" \
            --data-urlencode "reply_to=${_reply_to}" 2>/dev/null)
        # 28 = curl 逾時：daemon 可能已經排進佇列、只是回得慢。這時退回直送會讓同一句又打進 AGM 的
        # pane。再問一次、給久一點——沒帶 request id 的申請 daemon 以內容指紋去重，重問不會變兩筆。
        # 連不上（daemon 不在）才照舊直送。
        if [ "$?" = 28 ]; then
            _resp=$(curl -s -m 15 -X POST "http://127.0.0.1:${AM_PORT}/relay/announce" \
                -H "X-AM-Bot-Token: ${AM_HOOK_TOKEN}" \
                --data-urlencode "bot_id=${AM_BOT_ID}" \
                --data-urlencode "to_agent=${_name}" \
                --data-urlencode "text=${_text}" \
                --data-urlencode "ack=${_ack}" \
                --data-urlencode "reply_to=${_reply_to}" 2>/dev/null) || _resp=""
        fi
        # 寫給 AGM 的申請 daemon 已經排進協調者的佇列（SPEC §18.15）：不再打進 AGM 的 pane，
        # 否則同一句話會先燒一輪巡檢的回合。daemon 沒回應時照舊送（寧可多一回合，不能掉訊息）。
        case "$_resp" in
            *'"routed":'*)
                printf 'agents-manager: 已排入 AGM 協調佇列，不直接打進 %s 的 pane：%s\n' "$_name" "$_resp" >&2
                exit 0
                ;;
        esac
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
                        AM_INSTANCE=* | AM_DATA_DIR=*)
                            shift
                            _i=$((_i + 1))
                            continue
                            ;;
                    esac
                fi
                ;;
            --env=AM_INSTANCE=* | --env=AM_DATA_DIR=*) continue ;;
        esac
        set -- "$@" "$_a"
    done
    for _k in AM_INSTANCE AM_DATA_DIR; do
        eval "_v=\${$_k:-}"
        [ -z "$_v" ] || set -- "$@" --env "$_k=$_v"
    done
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
        case " $_seen " in
            *" $_k "*) continue ;;
        esac
        set -- "$@" --env "$_k=$_v"
    done
    # §6.5e：開出來的 pane 要能說出「這是誰、為了什麼開的」。歸屬由 daemon 從行程環境推斷（AM_BOT_ID
    # 一定帶得下去），這裡只補**用途**：`--purpose <文字>` 是我們自己的旗標，轉發前剝掉。
    # 沒有 curl／沒有 token 就只是少一個字串，pane 照開。
    if [ -n "$_purpose" ] && [ -n "${AM_BOT_ID:-}" ] && [ -n "${AM_HOOK_TOKEN:-}" ] && [ -n "${AM_PORT:-}" ] && command -v curl >/dev/null 2>&1; then
        _out=$("$AM_HERDR" "$_sub1" "$_sub2" "$@")
        _rc=$?
        printf '%s\n' "$_out"
        [ "$_rc" -eq 0 ] || exit "$_rc"
        # 回應裡第一個 pane_id 就是剛開出來的那顆（herdr 0.8.2 schema：pane_created 只有 `pane`，tab_created／
        # workspace_created 只有 `root_pane` 帶 pane_id）。冒號兩邊有沒有空白都認。
        _pane=$(printf '%s' "$_out" | tr ',' '\n' | sed -n 's/.*"pane_id" *: *"\([^"]*\)".*/\1/p' | head -n 1)
        [ -n "$_pane" ] || exit 0
        curl -s -m 2 -X POST "http://127.0.0.1:${AM_PORT}/relay/pane" \
            -H "X-AM-Bot-Token: ${AM_HOOK_TOKEN}" \
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

/// Same ssh path as `hook.sh` (SPEC §11.4).
pub async fn install_remote(conn: &crate::hosts::HostConn, remote_bot_dir: &str) -> anyhow::Result<String> {
    let dir = format!("{remote_bot_dir}/bin");
    let script = format!(
        "set -e\nD={dir}\nmkdir -p \"$D\"\ncat > \"$D/herdr\" <<'AM_SHIM_EOF'\n{shim}AM_SHIM_EOF\nchmod +x \"$D/herdr\"\nprintf 'AM_SHIM_INSTALLED\\n'\n",
        dir = crate::hosts::sh_quote(&dir),
        shim = SHIM_SH,
    );
    let out = conn.ssh_exec(&script).await?;
    if !out.contains("AM_SHIM_INSTALLED") {
        anyhow::bail!("remote herdr shim install did not confirm:\n{}", out.trim());
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    //! Runs the real script against a fake `herdr` that prints argv one per line.
    use std::io::Write as _;
    use std::process::Command;

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
            let mut f = std::fs::File::create(fake.join("herdr")).unwrap();
            // `agent get <name>` answers from `AM_TEST_AGENTS`; everything else echoes argv.
            f.write_all(
                b"#!/bin/sh\n\
                  if [ \"$1\" = agent ] && [ \"$2\" = get ]; then\n\
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
                  for a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
            )
            .unwrap();
            drop(f);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(fake.join("herdr"), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Sandbox { dir }
        }

        fn run(&self, env: &[(&str, &str)], args: &[&str]) -> (Vec<String>, String) {
            let mut cmd = Command::new(self.dir.join("bin/herdr"));
            let path = format!(
                "{}:{}:/usr/bin:/bin",
                self.dir.join("bin").display(),
                self.dir.join("real").display()
            );
            cmd.env("PATH", path).args(args);
            // Don't inherit the test runner's own pane model settings.
            // 在 bot 的 pane 裡跑測試時，AM_BOT_ID／AM_HOOK_TOKEN／AM_PORT 都有值，shim 會真的去打
            // 正在跑的 daemon——而雙角色上線之後，daemon 會把寫給 AGM 的那句攔進佇列、shim 不再轉給
            // herdr，測試就看到空輸出。需要這幾個值的測試自己設。
            for key in ["AM_MODEL", "AM_EFFORT", "AM_KIND", "AM_BOT_ID", "AM_HOOK_TOKEN", "AM_PORT", "AM_INSTANCE", "AM_DATA_DIR", "AM_OUTBOX", "HERDR_PANE_ID", "AM_WORKSPACE_ID", "AM_DAEMON_EXE", "AM_CONFIG_PATH"] {
                cmd.env_remove(key);
            }
            for (k, v) in env {
                cmd.env(k, v);
            }
            let out = cmd.output().unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).lines().map(String::from).collect();
            (stdout, String::from_utf8_lossy(&out.stderr).into_owned())
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
        std::fs::write(&fake_curl, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n", log.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&fake_curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let created = r#"{"id":"cli:tab:create","result":{"root_pane":{"agent":null,"pane_id": "w5:p9","tab_id":"w5:t4","workspace_id":"w5"},"tab":{"label":"sh","tab_id":"w5:t4","workspace_id":"w5"},"type":"tab_created"}}"#;
        let env = [("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "1"), ("AM_TEST_CREATE_JSON", created)];
        let (out, _) = s.run(&env, &["tab", "create", "--workspace", "w5", "--purpose", "dev-server"]);
        assert_eq!(out, [created.to_string()], "herdr 的輸出原樣印出");
        let sent = std::fs::read_to_string(&log).unwrap();
        assert!(sent.lines().any(|l| l == "pane_id=w5:p9"), "{sent}");
        assert!(sent.lines().any(|l| l == "purpose=dev-server"), "{sent}");
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
        ];
        s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9"]);
        let sent = std::fs::read_to_string(&log).unwrap();
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
        std::fs::write(&fake_curl, "#!/bin/sh\nprintf '%s' \"${AM_TEST_CURL_REPLY:-}\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&fake_curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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

    /// `--ack`／`--reply-to` 是我們的旗標（review 2026-09-16 H1：寄件端明講才算回覆）：送給 daemon、不給真的
    /// herdr。herdr 自己的 `--wait --timeout` 不算正文。curl 逾時（28）先再問一次，不直接打進 AGM 的 pane。
    #[test]
    fn reply_marks_go_to_the_daemon_and_a_slow_daemon_is_asked_again_before_falling_back() {
        let s = Sandbox::new();
        let log = s.dir.join("curl.log");
        let count = s.dir.join("curl.count");
        let fake_curl = s.dir.join("real").join("curl");
        // 每次呼叫把 --data-urlencode 的值一行一行記下來；第一次照 AM_TEST_CURL_FIRST_RC 結束。
        std::fs::write(
            &fake_curl,
            format!(
                "#!/bin/sh\n\
                 n=$(cat '{count}' 2>/dev/null || echo 0); n=$((n + 1)); echo $n > '{count}'\n\
                 prev=''\n\
                 for a in \"$@\"; do [ \"$prev\" = --data-urlencode ] && printf '%s\\n' \"$a\" >> '{log}'; prev=$a; done\n\
                 echo --- >> '{log}'\n\
                 if [ \"$n\" = 1 ] && [ -n \"${{AM_TEST_CURL_FIRST_RC:-}}\" ]; then exit \"$AM_TEST_CURL_FIRST_RC\"; fi\n\
                 printf '%s' \"${{AM_TEST_CURL_REPLY:-}}\"\n",
                count = count.display(),
                log = log.display(),
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&fake_curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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
        let (out, _) = s.run(&[("AM_AGENT_NAME", "proj-abc123")], &["agent", "start", &"x".repeat(40)]);
        assert_eq!(out[2].len(), 32);
        assert!(out[2].starts_with("proj-abc123-x"));
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
        ];
        let (out, _) = s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9"]);
        assert_eq!(out, ["agent", "start", "p-1-kid", "--kind", "claude", "--pane", "w1:p9"], "agent start 的 argv 不變（herdr 沒有 --env）");
        let sent = std::fs::read_to_string(&log).unwrap();
        let calls: Vec<&str> = sent.split("---\n").filter(|c| !c.trim().is_empty()).collect();
        assert_eq!(calls.len(), 1, "只補一次：{sent}");
        let lines: Vec<&str> = calls[0].lines().collect();
        assert_eq!(&lines[..3], ["pane", "send-text", "w1:p9"], "補的是 --pane 指到的目標：{lines:?}");
        let text = lines[3..].join("\n");
        assert!(text.contains("export CLAUDE_CONFIG_DIR='/home/u/.claude-cc2'"), "{text}");
        assert!(text.contains("export AM_BOT_ID='b1'"), "{text}");
        assert!(text.contains("export AM_HOOK_TOKEN='tok'"), "{text}");
        assert!(text.contains("export AM_INSTANCE='a1b2'"), "隔離實例的保留變數也補：{text}");
        assert!(text.contains("export AM_DATA_DIR='/data/iso'"), "{text}");
        assert!(text.ends_with('\n'), "trailing newline 讓 pty 送出這行：{text:?}");
    }

    /// 值裡有單引號要逃脫，不然那個 export 的邊界會斷在半路。
    #[test]
    fn agent_start_reexport_escapes_single_quotes_in_values() {
        let s = Sandbox::new();
        let log = s.dir.join("sendtext.log");
        let env = [("AM_AGENT_NAME", "p-1"), ("AM_OUTBOX", "/data/o'tbox"), ("AM_TEST_SENDTEXT_LOG", log.to_str().unwrap())];
        s.run(&env, &["agent", "start", "kid", "--kind", "claude", "--pane", "w1:p9"]);
        let sent = std::fs::read_to_string(&log).unwrap();
        assert!(sent.contains(r"export AM_OUTBOX='/data/o'\''tbox'"), "{sent}");
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
}
