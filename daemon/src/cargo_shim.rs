//! The `cargo` PATH shim (issue #90): global build-scheduler admission for every managed bot's
//! `build`/`check`/`test`/`clippy`/… so agents keep typing plain `cargo …` while the daemon caps
//! host-wide rustc concurrency, instead of every bot copying a hand-run `cargo-slot.sh`.
//!
//! Installed into the **same** bin dir as the `herdr` shim (`herdr_shim::install_local`/`install_remote`
//! write to it too), so one PATH prepend covers both.

use std::path::{Path, PathBuf};

pub const SHIM_SH: &str = r##"#!/bin/sh
# agents-manager cargo build-slot shim (issue #90). Installed at the front of a managed pane's PATH,
# next to the herdr shim.
#
# POSIX sh only, no `set -e`: a shim that aborts a build because ITS OWN scheduling call failed is
# worse than one that just runs the build unscheduled (SPEC herdr_shim.rs 的同一條原則)。
#
# AM_SHIM_MARKER: 這行讓 shim 認得出「PATH 上那個 cargo 其實是我自己」（am_is_shim）。不要刪。

# 這個檔案是不是這支 shim 的另一份拷貝。`AM_SHIM_MARKER` 只出現在 shim 自己的檔頭。
am_is_shim() {
    head -n 12 "$1" 2>/dev/null | grep -q 'AM_SHIM_MARKER' 2>/dev/null
}

# 這個目錄是不是某顆 bot 的 shim 目錄（`…/bots/<id>/bin`，見 daemon 的 `shim_path`）。
#
# 檔頭認不出來時的第二道：**舊版的 shim 沒有 `AM_SHIM_MARKER`**。換版期間 PATH 上同時有新舊兩份
# 是常態（shim 是 bot 啟動時才寫的），舊的那份對新的 shim 來說就是「一個普通的 cargo」——2026-09-18
# 18:40 那次就是這樣，一條呼叫鏈上 5 個 shim 互等名額。整個目錄跳掉就不必看內容。
am_is_bot_bin_dir() {
    case "${1%/}" in
        */bots/*/bin) return 0 ;;
        *) return 1 ;;
    esac
}

# The real cargo: `$AM_REAL_CARGO` if set, else the first `cargo` on PATH that is not a copy of this
# shim.
#
# **不能只跳過自己那個目錄**：一顆 bot 的 pane 會繼承祖先 pane 的 PATH，同一條 PATH 上常常掛著
# 好幾顆 bot 的 `bots/<id>/bin`（2026-09-18 實測有 6 個）。只比對自己的目錄時，下一個目錄裡的
# 「cargo」就是同一支 shim，於是 shim → shim → shim 一層一層都去拿名額：`max_concurrent=2` 被自己
# 的外層佔滿，最內層那個永遠等不到，整台機器明明是空的卻卡死（w168:p91 卡了 17 分鐘）。
am_real_cargo() {
    if [ -n "${AM_REAL_CARGO:-}" ] && [ -x "$AM_REAL_CARGO" ] && ! am_is_shim "$AM_REAL_CARGO" \
        && ! am_is_bot_bin_dir "$(dirname "$AM_REAL_CARGO")"; then
        printf '%s\n' "$AM_REAL_CARGO"
        return 0
    fi
    _self=$(cd "$(dirname "$0")" 2>/dev/null && pwd)
    printf '%s\n' "$PATH" | tr ':' '\n' | {
        while IFS= read -r _d; do
            [ -n "$_d" ] || _d=.
            _abs=$(cd "$_d" 2>/dev/null && pwd) || continue
            [ "$_abs" = "$_self" ] && continue
            am_is_bot_bin_dir "$_abs" && continue
            if [ -x "$_abs/cargo" ] && ! am_is_shim "$_abs/cargo"; then
                printf '%s\n' "$_abs/cargo"
                break
            fi
        done
    }
}

# `build`/`check`/`test`/`clippy`/… multiply rustc processes; `--version`/`metadata`/`tree`/`fmt`/
# `fetch` don't compile anything and would just add latency for nothing.
am_cargo_is_heavy() {
    case "$1" in
        b | build | c | check | t | test | clippy | bench | r | run | rustc | doc | install) return 0 ;;
        *) return 1 ;;
    esac
}

# issue #104：跨平台 V1 只 offload verification。build/run 留本機，因為 Linux/x86_64 artifact
# 不能拿回 Apple Silicon macOS 當成本機 binary 用。
am_remote_cargo_eligible() {
    case "$1" in
        c | check | t | test | clippy) return 0 ;;
        *) return 1 ;;
    esac
}

# 外部編譯（`remote-cargo` helper）需要的三樣東西；缺哪個就回哪個的名字，空字串＝齊了。
# `AM_DAEMON_EXE` 指到的檔案不能執行（binary 被換掉／搬走）也算缺。
am_remote_cargo_missing() {
    _miss=""
    { [ -n "${AM_DAEMON_EXE:-}" ] && [ -x "$AM_DAEMON_EXE" ]; } || _miss="$_miss AM_DAEMON_EXE"
    [ -n "${AM_CONFIG_PATH:-}" ] || _miss="$_miss AM_CONFIG_PATH"
    [ -n "${AM_DATA_DIR:-}" ] || _miss="$_miss AM_DATA_DIR"
    printf '%s' "${_miss# }"
}

# `http://127.0.0.1:$AM_PORT` — bots always have `AM_PORT`; a manual host shell defaults to the
# documented port (SPEC: daemon 在 127.0.0.1:7788)。
am_build_port() {
    printf '%s' "${AM_PORT:-7788}"
}

# 認證：bot 用自己的 hook token（pane 裡本來就有），人工 host shell 用一般 UI token（讀
# `~/.config/agents-manager/ui-token`，daemon 的預設位置）。兩個都沒有就回傳空字串，呼叫端看到空字串
# 就該直接跳過排程——沒有身分，daemon 也不可能認得這個名額。
am_build_auth_header() {
    if [ -n "${AM_BOT_ID:-}" ] && [ -n "${AM_HOOK_TOKEN:-}" ]; then
        printf 'X-AM-Bot-Token: %s' "$AM_HOOK_TOKEN"
        return 0
    fi
    _tok_file="$HOME/.config/agents-manager/ui-token"
    if [ -r "$_tok_file" ]; then
        _tok=$(cat "$_tok_file" 2>/dev/null)
        if [ -n "$_tok" ]; then
            printf 'X-AM-Token: %s' "$_tok"
            return 0
        fi
    fi
    return 1
}

# `sed` 從 JSON 回應挖一個欄位（herdr_shim.rs 同一招：不上 jq 依賴，冒號兩邊有沒有空白都認）。
am_json_field() {
    printf '%s' "$2" | tr ',' '\n' | sed -n 's/.*"'"$1"'" *: *"\{0,1\}\([^",}]*\)"\{0,1\}.*/\1/p' | head -n 1
}

# ── 名額租約的守衛（issue #128）──────────────────────────────────────────────────────────
#
# 名額是 TTL 租的：daemon 端在停止續約超過 TTL 之後會把它收回、讓別人拿。所以持有者有兩件事必須同時成立
# ——daemon 能收回死掉的持有者的容量，**而且活著但租約已失效的持有者必須停止使用容量**。少了後半，
# 續約一斷（daemon 重啟、暫時連不上、名額被收）前景的 cargo 照跑，別人拿到同一個名額也開始編，
# `max_concurrent` 就被突破。

# 現在的 epoch 秒；`date +%s` 讀不出整數就回失敗（呼叫端要往保守方向走）。
am_epoch() {
    _e=$(date +%s 2>/dev/null)
    case "$_e" in '' | *[!0-9]*) return 1 ;; esac
    printf '%s' "$_e"
}

# `$1` 底下所有後代的 pid（同一份 ps 快照、不含 `$1` 自己），一行一個。
am_descendants() {
    ps -A -o pid= -o ppid= 2>/dev/null | awk -v root="$1" '
        { kids[$2] = kids[$2] " " $1 }
        END {
            n = 1; q[1] = root
            for (h = 1; h <= n; h++) {
                c = split(kids[q[h]], k, " ")
                for (i = 1; i <= c; i++) { q[++n] = k[i]; print k[i] }
            }
        }'
}

# 把 `$1`（必須還是這支 shim 的直接子行程）整棵行程樹停掉，一顆 rustc 都不留。
#
# 先 SIGSTOP 凍住整棵、重拍快照直到不再長新的：cargo 在「拍快照」與「送訊號」之間還會生出新的 rustc，
# 而它的父親一死就被 init 收養、從此按父子關係追不到（＝孤兒編譯器，正是最不能留的東西）。
# 凍住之後再 TERM＋CONT 讓它們正常收尾，兩秒後還活著的補 KILL。
am_kill_tree() {
    _kt_ppid=$(ps -o ppid= -p "$1" 2>/dev/null | tr -d ' ')
    # 已經退出（pid 可能被別的行程用掉了）就不要亂殺。
    [ "$_kt_ppid" = "$$" ] || return 0
    _kt_set="$1 $(am_descendants "$1" | tr '\n' ' ')"
    _kt_round=0
    while [ "$_kt_round" -lt 5 ]; do
        kill -STOP $_kt_set 2>/dev/null
        _kt_grew=""
        for _kt_p in $(am_descendants "$1"); do
            case " $_kt_set " in
                *" $_kt_p "*) ;;
                *) _kt_set="$_kt_set $_kt_p"; _kt_grew=1 ;;
            esac
        done
        [ -n "$_kt_grew" ] || break
        _kt_round=$((_kt_round + 1))
    done
    kill -TERM $_kt_set 2>/dev/null
    kill -CONT $_kt_set 2>/dev/null
    # 寬限最多兩秒：TERM 之後一秒內都死光了就不必多等，還活著的才補 KILL。
    _kt_wait=0
    while [ "$_kt_wait" -lt 2 ]; do
        sleep 1
        _kt_alive=""
        for _kt_p in $_kt_set; do
            kill -0 "$_kt_p" 2>/dev/null && _kt_alive="$_kt_alive $_kt_p"
        done
        [ -n "$_kt_alive" ] || return 0
        _kt_wait=$((_kt_wait + 1))
    done
    kill -KILL $_kt_alive 2>/dev/null
    return 0
}

# 租約失效：先留下標記（前景那邊靠它分辨「被我們停的」與「自己失敗的」），再停掉前景的行程樹。
am_lease_lost() {
    printf '%s\n' "$1" > "$_state/lost"
    _lost_pid=$(cat "$_state/pid" 2>/dev/null)
    [ -z "$_lost_pid" ] || am_kill_tree "$_lost_pid"
    return 0
}

# 可以被 TERM 打斷的 `sleep`：`_release` 殺守衛（子 shell）時，它手上的 `sleep N` 不會變成孤兒（行程數是全機共用的資源，
# 每次 cargo 都留一顆 `sleep 60` 的孤兒，很快就把機器的行程表塞滿——issue #151）。
am_lease_sleep() {
    sleep "$1" &
    _ls_pid=$!
    wait "$_ls_pid"
    _ls_pid=""
}

# 背景執行：續約，並判斷租約還在不在。
#
# * daemon 明確說沒有這個名額（`not_found`）或 token 對不上（`token_mismatch`）：名額已經不是我們的，立刻停。
# * 其他一切失敗（連不上、逾時、5xx、看不懂的回應）：先當暫時的，續約還有機會就繼續試；
#   但**不能等到 daemon 的到期時間之後才動手**——那時別人可能已經拿到同一個名額。所以估一個保守的
#   deadline（＝續約成功那個請求**送出**的時間 ＋ TTL，daemon 記的到期一定不早於它），
#   下一次重試（隔 `_renew_every`、最久再加 curl 逾時）會落在 deadline 之後就現在停。
# * 讀不到時間（`date +%s` 壞掉）：沒有依據可以等，第一次失敗就停。
am_lease_watch() {
    _lw_shim=$$
    _ls_pid=""
    trap '[ -z "$_ls_pid" ] || kill "$_ls_pid" 2>/dev/null; exit 0' TERM
    # 續約請求的逾時：隔多久續一次的一半，夾在 1～5 秒（正常的 TTL 下就是 5 秒）。
    _lw_m=$((_renew_every / 2))
    [ "$_lw_m" -ge 1 ] || _lw_m=1
    [ "$_lw_m" -le 5 ] || _lw_m=5
    while :; do
        am_lease_sleep "$_renew_every"
        # shim 本身死了（被 SIGKILL，`trap` 沒機會跑）：沒有人會來放名額。cargo 還在跑就繼續續約
        # （它還在用容量，租約不能先掉）；cargo 也沒了就把名額放掉、收掉狀態目錄、自己結束——
        # 不然這個迴圈會永遠續下去，把名額佔到重開機。
        if ! kill -0 "$_lw_shim" 2>/dev/null; then
            _lw_c=$(cat "$_state/pid" 2>/dev/null)
            if [ -z "$_lw_c" ] || ! kill -0 "$_lw_c" 2>/dev/null; then
                curl -s -m 5 -X POST "${_url}/release" --data-urlencode "holder=${_holder}" --data-urlencode "token=${_token}" >/dev/null 2>&1
                rm -rf "$_state" 2>/dev/null
                return 0
            fi
        fi
        _lw_sent=$(am_epoch)
        _lw_resp=$(curl -s -m "$_lw_m" -X POST "${_url}/renew" --data-urlencode "holder=${_holder}" --data-urlencode "token=${_token}" 2>/dev/null)
        if [ "$(am_json_field renewed "$_lw_resp")" = "true" ]; then
            [ -z "$_lw_sent" ] || _deadline=$((_lw_sent + _ttl))
            continue
        fi
        _lw_err=$(am_json_field error "$_lw_resp")
        case "$_lw_err" in
            not_found | token_mismatch)
                am_lease_lost "daemon 已不承認這個名額（${_lw_err}）"
                return 0
                ;;
        esac
        _lw_now=$(am_epoch)
        if [ -z "$_lw_now" ] || [ -z "$_deadline" ] || [ $((_deadline - _lw_now)) -le $((_renew_every + _lw_m)) ]; then
            am_lease_lost "續約一直失敗，名額快到期了"
            return 0
        fi
    done
}

# 前景跑指令的包裝：先把自己的 pid 寫給守衛，再 `exec` 成那個指令（pid 不變，所以記下來的就是 cargo 本人）。
# 前景執行不能換成背景：非互動 shell 的背景指令會忽略 SIGINT／SIGQUIT（Ctrl-C 就停不下 cargo，
# `cargo run` 起來的程式也繼承到「忽略 SIGINT」），stdin 也會被換成 /dev/null。
_guard_sh='printf "%s" "$$" > "$1" 2>/dev/null; shift; exec "$@"'

am_cargo() {
    # 遞迴保險絲：萬一 `am_is_shim` 認不出某一份 shim（被改過檔頭、或別的專案裝了同名 wrapper），
    # 兩份 shim 會互相把對方當成真 cargo 一路 fork 下去。寧可大聲失敗，也不要 fork 到機器躺平。
    AM_SHIM_DEPTH=$((${AM_SHIM_DEPTH:-0} + 1))
    export AM_SHIM_DEPTH
    _parent=$PPID
    if [ "$AM_SHIM_DEPTH" -gt 4 ]; then
        printf 'agents-manager: cargo shim 遞迴 %s 層——PATH 上有多份 shim 而且認不出來。把真 cargo 放進 AM_REAL_CARGO 再跑一次。\n' "$AM_SHIM_DEPTH" >&2
        exit 127
    fi
    _real=$(am_real_cargo | head -n 1)
    if [ -z "$_real" ]; then
        printf 'agents-manager: 找不到真正的 cargo（把它的路徑放進 AM_REAL_CARGO）\n' >&2
        exit 127
    fi
    if ! am_cargo_is_heavy "${1:-}" || ! command -v curl >/dev/null 2>&1; then
        exec "$_real" "$@"
    fi
    # 這條進程鏈上已經有人拿著名額（build script 或 xtask 再叫一次 cargo）：直接跑，不要再排一次。
    # 內層等的名額只會等到外層結束才空出來，而外層在等內層——就是上面那個死結。
    if [ -n "${AM_BUILD_SLOT_HELD:-}" ]; then
        AM_REAL_CARGO="$_real"
        export AM_REAL_CARGO
        exec "$_real" "$@"
    fi
    _auth=$(am_build_auth_header) || {
        printf 'agents-manager: 沒有 bot／管理員身分，這次 cargo 不經過排程器，直接跑\n' >&2
        exec "$_real" "$@"
    }
    _port=$(am_build_port)
    _holder="${AM_AGENT_NAME:-manual}:$$"
    _bot_id="${AM_BOT_ID:-}"
    _purpose=$(printf '%s' "$*" | cut -c1-200)
    _url="http://127.0.0.1:${_port}/build-slots"

    _attempt=0
    while :; do
        _acq_at=$(am_epoch)
        _resp=$(curl -s -m 5 -X POST "${_url}/acquire" \
            -H "$_auth" \
            --data-urlencode "holder=${_holder}" \
            --data-urlencode "bot_id=${_bot_id}" \
            --data-urlencode "purpose=${_purpose}" 2>/dev/null)
        _rc=$?
        if [ "$_rc" -ne 0 ] || [ -z "$_resp" ]; then
            printf 'agents-manager: build scheduler 連不上（daemon 沒開？），這次不排程，直接跑\n' >&2
            exec "$_real" "$@"
        fi
        case "$_resp" in
            *'"granted":true'*) break ;;
            *'"granted":false'*)
                _attempt=$((_attempt + 1))
                if [ "$_attempt" = 1 ]; then
                    printf 'agents-manager: 全機的 cargo 名額滿了，等一個空出來（waiting_for_build_slot）……\n' >&2
                fi
                # 呼叫端（叫我們的 shell／agent）已經不在了：沒有人在等這次建置，不要留一個永遠在等名額的孤兒。
                if ! kill -0 "$_parent" 2>/dev/null; then
                    printf 'agents-manager: 呼叫端已經結束，不再等 cargo 名額\n' >&2
                    exit 1
                fi
                _retry=$(am_json_field retry_after_secs "$_resp")
                case "$_retry" in *[!0-9]* | '') _retry=5 ;; esac
                sleep "$_retry"
                continue
                ;;
            *)
                printf 'agents-manager: build scheduler 回應看不懂，這次不排程，直接跑：%s\n' "$_resp" >&2
                exec "$_real" "$@"
                ;;
        esac
    done

    _token=$(am_json_field token "$_resp")
    _jobs=$(am_json_field cargo_jobs "$_resp")
    _ttl=$(am_json_field lease_ttl_secs "$_resp")
    case "$_jobs" in *[!0-9]* | '') _jobs=2 ;; esac
    case "$_ttl" in *[!0-9]* | '') _ttl=180 ;; esac
    # 續約間隔取 TTL 的三分之一：daemon 端的 sweep 也是等好幾個間隔才收，一次沒續到不會立刻掉名額。
    _renew_every=$((_ttl / 3))
    [ "$_renew_every" -ge 1 ] || _renew_every=1
    # 保守的到期時間：daemon 記的到期是「收到請求那一刻 ＋ TTL」，一定不早於「送出請求那一刻 ＋ TTL」。
    _deadline=""
    [ -z "$_acq_at" ] || _deadline=$((_acq_at + _ttl))

    _released=""
    _release() {
        [ -z "$_released" ] || return 0
        _released=1
        kill "$_renew_pid" 2>/dev/null
        curl -s -m 5 -X POST "${_url}/release" --data-urlencode "holder=${_holder}" --data-urlencode "token=${_token}" >/dev/null 2>&1
        rm -rf "$_state" 2>/dev/null
        return 0
    }

    # 守衛要有地方留 pid 與「租約失效」標記；建不起來就沒辦法在租約失效時停掉 cargo，
    # 那就別拿名額（放回去、不排程直接跑），不要拿了名額卻不受它管。
    _state=$(mktemp -d "${TMPDIR:-/tmp}/am-cargo-lease.XXXXXX" 2>/dev/null)
    if [ -z "$_state" ] || [ ! -d "$_state" ]; then
        curl -s -m 5 -X POST "${_url}/release" --data-urlencode "holder=${_holder}" --data-urlencode "token=${_token}" >/dev/null 2>&1
        printf 'agents-manager: 建不出暫存目錄，沒辦法在名額失效時停掉 cargo，這次不排程，直接跑\n' >&2
        exec "$_real" "$@"
    fi
    am_lease_watch &
    _renew_pid=$!
    trap '_release' EXIT INT TERM

    # 租約已經失效（前景那個被我們停掉，或是在兩段指令之間失效）：收尾、告訴使用者為什麼、退 75（可重試）。
    # 等背景那個把行程樹收乾淨才放手（它還在等 TERM 之後的兩秒寬限，之後才補 KILL）。
    am_lease_lost_exit() {
        wait "$_renew_pid" 2>/dev/null
        _why=$(cat "$_state/lost" 2>/dev/null)
        _release
        trap - EXIT INT TERM
        printf 'agents-manager: cargo 的建置名額租約失效了（%s），已經把這次建置與它底下所有行程停掉；重跑一次就會重新排隊。\n' "$_why" >&2
        exit 75
    }
    # 前景指令結束後：只有「標記在、而且它是被砍掉的（非 0）」才算租約失效；標記在但它已經正常跑完就照它的結果。
    am_lease_lost_now() {
        [ -s "$_state/lost" ] && [ "$1" -ne 0 ]
    }

    # 外部 Cargo worker：helper 本身讀 config + 0600 secret file，pane 不會拿到 SSH 密碼。
    # 125 = 設定在 pane 啟動後被關掉／這個指令不適合 offload，退回本機 cargo；
    # 其他非 0 = 遠端驗證真的失敗，原樣回報，不能偷偷改成本機成功。
    #
    # pane 缺 helper／config 的位置時**不能靜默退回本機**（issue #138）：整批子 agent 因此在本機排隊，
    # 而沒有人知道外部編譯根本沒生效。缺哪個就講哪個。
    if am_remote_cargo_eligible "${1:-}"; then
        _remote_missing=$(am_remote_cargo_missing)
        if [ -n "$_remote_missing" ]; then
            printf 'agents-manager: 外部編譯沒有啟用：這個 pane 缺 %s（沒設，或指到的檔案不能執行），這次 %s 在本機跑\n' "$_remote_missing" "${1:-cargo}" >&2
        else
            sh -c "$_guard_sh" am-guarded "$_state/pid" "$AM_DAEMON_EXE" remote-cargo --config "$AM_CONFIG_PATH" --data-dir "$AM_DATA_DIR" --cwd "$PWD" -- "$@"
            _remote_rc=$?
            if am_lease_lost_now "$_remote_rc"; then
                am_lease_lost_exit
            fi
            if [ "$_remote_rc" -ne 125 ]; then
                _release
                trap - EXIT INT TERM
                exit "$_remote_rc"
            fi
        fi
    fi

    # 沒有名額就不起本機 cargo（例如 remote-cargo 途中租約失效、helper 又剛好回 125）。
    if [ -s "$_state/lost" ]; then
        am_lease_lost_exit
    fi

    env AM_BUILD_SLOT_HELD=1 AM_REAL_CARGO="$_real" CARGO_BUILD_JOBS="$_jobs" \
        sh -c "$_guard_sh" am-guarded "$_state/pid" "$_real" "$@"
    _cargo_rc=$?
    if am_lease_lost_now "$_cargo_rc"; then
        am_lease_lost_exit
    fi
    _release
    trap - EXIT INT TERM
    exit "$_cargo_rc"
}

am_cargo "$@"
"##;

/// Rewritten every start, same as the herdr shim: an upgraded daemon never leaves an old one behind.
pub fn install_local(bot_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = crate::shim_refresh::bin_dir(bot_dir);
    std::fs::create_dir_all(&dir)?;
    // 暫存檔 + rename：直接覆寫的話，正在跑的那支 shim 會讀到寫到一半的內容，而且中途死掉會留下
    // 一個不能執行的檔案（`shim_refresh::write_atomic`）。內容一樣就不動。
    crate::shim_refresh::write_atomic(&dir.join("cargo"), SHIM_SH)?;
    Ok(dir)
}

/// Same ssh path as the herdr shim (SPEC §11.4)，同一支原子同步腳本（issue #124）。
pub async fn install_remote(conn: &crate::hosts::HostConn, remote_bot_dir: &str) -> anyhow::Result<String> {
    let r = crate::shim_refresh::sync_remote(conn, &[remote_bot_dir.to_string()], &["cargo"], true).await?;
    if !r.failed.is_empty() {
        anyhow::bail!("remote cargo shim install failed: {:?}", r.failed);
    }
    Ok(format!("{remote_bot_dir}/bin"))
}

#[cfg(test)]
mod tests {
    //! Runs the real script against a fake `cargo`/`curl`, mirroring `herdr_shim.rs`'s Sandbox.
    use std::io::Write as _;
    use std::process::Command;

    /// 剛寫好的腳本立刻 `exec`，可能撞上 `ETXTBSY`（Text file busy）：並行的測試在別的執行緒 `fork`，
    /// 短暫繼承了那個檔案的寫入 fd，直到它自己 `exec`。這不是被測程式的問題，重試就好。
    fn output_retrying(cmd: &mut Command) -> std::process::Output {
        for _ in 0..200 {
            match cmd.output() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => std::thread::sleep(std::time::Duration::from_millis(25)),
                r => return r.unwrap(),
            }
        }
        cmd.output().unwrap()
    }

    fn spawn_retrying(cmd: &mut Command) -> std::process::Child {
        for _ in 0..200 {
            match cmd.spawn() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => std::thread::sleep(std::time::Duration::from_millis(25)),
                r => return r.unwrap(),
            }
        }
        cmd.spawn().unwrap()
    }

    struct Sandbox {
        dir: std::path::PathBuf,
        /// 這個沙盒起過的每一次 shim 的 process group（issue #151）：Drop（含測試 panic）時整組終止。
        groups: std::sync::Mutex<Vec<i32>>,
    }

    /// 這個 process group 裡還活著的行程（`pid`、指令），殭屍不算。
    fn group_members(pgid: i32) -> Vec<(i32, String)> {
        let out = Command::new("ps").args(["-A", "-o", "pid=,pgid=,stat=,command="]).output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let pid: i32 = it.next()?.parse().ok()?;
                let g: i32 = it.next()?.parse().ok()?;
                let stat = it.next()?;
                (g == pgid && !stat.starts_with('Z')).then(|| (pid, it.collect::<Vec<_>>().join(" ")))
            })
            .collect()
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            // 修正前的 shim 不會停掉它們：別讓失敗的測試留下五分鐘的孤兒 sleep。
            for f in ["cargo.pid", "rustc.pid"] {
                if let Some(pid) = self.pid(f) {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
            for pid in self.spawned() {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            for g in self.groups.lock().unwrap().iter() {
                unsafe { libc::killpg(*g, libc::SIGKILL) };
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Sandbox {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("am-cargo-shim-{}", crate::db::ulid()));
            std::fs::create_dir_all(&dir).unwrap();
            super::install_local(&dir).unwrap();
            let fake = dir.join("real");
            std::fs::create_dir_all(&fake).unwrap();
            let mut f = std::fs::File::create(fake.join("cargo")).unwrap();
            // Echoes argv and the env vars a test cares about, one per line, so assertions don't need a
            // real compiler. `$AM_TEST_FAKE_CARGO_LOG` records that the real cargo actually ran.
            f.write_all(
                b"#!/bin/sh\n\
                  { printf 'CARGO_BUILD_JOBS=%s\\n' \"${CARGO_BUILD_JOBS:-}\"; for a in \"$@\"; do printf '%s\\n' \"$a\"; done; } \
                    >> \"${AM_TEST_FAKE_CARGO_LOG:-/dev/null}\"\n\
                  exit \"${AM_TEST_FAKE_CARGO_EXIT:-0}\"\n",
            )
            .unwrap();
            drop(f);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(fake.join("cargo"), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Sandbox { dir, groups: Default::default() }
        }

        /// `AM_TEST_CURL_SCRIPT` is the fake curl's own body (appended after the shebang); tests write
        /// whatever behavior they need (record calls, answer with canned JSON, fail to simulate no daemon).
        fn install_fake_curl(&self, body: &str) {
            let path = self.dir.join("real").join("curl");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        /// 乾淨的一次 shim 呼叫：**呼叫端所有的 `AM_*` 都清掉**，不是列一份清單。這些測試跑在
        /// bot 的 pane 裡時，環境本來就有 `AM_DAEMON_EXE`／`AM_CONFIG_PATH`／`AM_DATA_DIR`（外部
        /// 編譯）、`AM_BOT_ID`、`AM_REAL_CARGO`…；漏進來的那一個就讓「假設某變數沒設」的測試必定
        /// 失敗，或讓 check 真的被 offload 到遠端主機（2026-09-19 build child 部署時兩種都中）。
        /// 名單永遠會漏，字首才不會；需要值的測試自己設。
        fn command(&self, path: &str) -> Command {
            self.command_in(None, path)
        }

        /// 同上，可以指定用哪個 shell 跑 shim（macOS 的 `/bin/sh`、`/bin/bash` 是 3.2，另有 `/bin/dash`）；`None` 照 shebang。
        fn command_in(&self, shell: Option<&str>, path: &str) -> Command {
            let mut cmd = match shell {
                Some(sh) => {
                    let mut c = Command::new(sh);
                    c.arg(self.dir.join("bin/cargo"));
                    c
                }
                None => Command::new(self.dir.join("bin/cargo")),
            };
            cmd.env("PATH", path);
            for (key, _) in std::env::vars() {
                if key.starts_with("AM_") {
                    cmd.env_remove(key);
                }
            }
            // 假的 $HOME：不能真的去讀開發機自己的 ui-token（會讓測試偷偷通過或偷偷失敗）。
            cmd.env("HOME", self.dir.join("fake-home"));
            cmd
        }


        /// 假 cargo 把自己與它的「rustc」子行程的 pid 寫在沙盒裡，跑到 `secs` 秒才寫 `cargo.done` 並正常結束。
        /// 子行程是一顆獨立的 `sleep 300`：cargo 死了它也不會自己結束，正好用來抓「留下孤兒編譯器」。
        /// 它的 stdout／stderr 導去 /dev/null——不然它握著測試捕捉輸出的 pipe，cargo 正常跑完時 `.output()` 會等到它結束。
        fn install_slow_cargo(&self, secs: u32) {
            let d = self.dir.display();
            let body = format!(
                "#!/bin/sh\necho $$ > '{d}/cargo.pid'\n( exec /bin/sleep 300 ) >/dev/null 2>&1 &\necho $! > '{d}/rustc.pid'\ntouch '{d}/cargo.ready'\n/bin/sleep {secs}\nkill $(cat '{d}/rustc.pid') 2>/dev/null\necho done > '{d}/cargo.done'\nexit 0\n"
            );
            let path = self.dir.join("real/cargo");
            std::fs::write(&path, body).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        /// 名額租約的假 daemon：acquire 一律 granted（TTL 由參數給），renew 依 `renew-mode` 檔決定：
        /// `ok`＝續約成功、`notfound`／`mismatch`＝明確拒絕、`down`＝連不上（curl 退 7）、
        /// `once_down`＝第一次連不上、之後都成功，`alternate`＝一次失敗、一次成功、交替下去。每一通都記在 `curl.log`。
        fn install_lease_curl(&self, ttl: u32) {
            let d = self.dir.display();
            self.install_fake_curl(&format!(
                r#"echo "$*" >> '{d}/curl.log'
case "$*" in
  *acquire*) printf '{{"granted":true,"token":"tok-1","cargo_jobs":2,"lease_ttl_secs":{ttl}}}' ;;
  *renew*)
    case "$(cat '{d}/renew-mode')" in
      ok) printf '{{"renewed":true,"expires_at":"x"}}' ;;
      notfound) printf '{{"error":"not_found","what":"build_slot"}}' ;;
      mismatch) printf '{{"error":"token_mismatch"}}' ;;
      down) exit 7 ;;
      once_down) if [ -f '{d}/once' ]; then printf '{{"renewed":true,"expires_at":"x"}}'; else touch '{d}/once'; exit 7; fi ;;
      alternate) if [ -f '{d}/flip' ]; then rm -f '{d}/flip'; printf '{{"renewed":true,"expires_at":"x"}}'; else touch '{d}/flip'; exit 7; fi ;;
    esac ;;
  *) printf '{{}}' ;;
esac"#
            ));
            self.set_renew_mode("ok");
        }

        /// 虛擬時鐘：把 PATH 上的 `sleep`／`date` 換成假的。`sleep N` 只把時鐘往前撥 N 秒（真的只睡 50ms，讓別的行程有機會跑），
        /// `date +%s` 讀這個時鐘。租約守衛的決定（要不要停）只看這兩個，所以測試**不吃機器負載**、不用真的等 TTL——
        /// 用真時鐘＋幾秒的 TTL 時，餘裕只有一兩秒，本機同時有人在編譯就會誤判。
        /// 只有背景的守衛在睡覺，時鐘只有一個寫入者。
        fn install_virtual_clock(&self) {
            let d = self.dir.display();
            std::fs::write(self.dir.join("clock"), "1000000").unwrap();
            let files = [
                (
                    "sleep",
                    // 等假 cargo 發出 ready 才撥時鐘：不然守衛在 cargo 還沒起來時就先判完了，殺樹的測試會空過。
                    format!(
                        "#!/bin/sh\ni=0\nwhile [ ! -e '{d}/cargo.ready' ] && [ $i -lt 500 ]; do /bin/sleep 0.02; i=$((i + 1)); done\nn=$(cat '{d}/clock')\necho $((n + ${{1%%.*}})) > '{d}/clock'\nexec /bin/sleep 0.05\n"
                    ),
                ),
                ("date", format!("#!/bin/sh\ncase \"$*\" in '+%s') cat '{d}/clock' ;; *) exec /bin/date \"$@\" ;; esac\n")),
            ];
            for (name, body) in files {
                let path = self.dir.join("real").join(name);
                std::fs::write(&path, body).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
        }

        /// 虛擬時鐘走了幾秒。
        fn virtual_secs(&self) -> i64 {
            std::fs::read_to_string(self.dir.join("clock")).unwrap().trim().parse::<i64>().unwrap() - 1_000_000
        }

        fn set_renew_mode(&self, mode: &str) {
            std::fs::write(self.dir.join("renew-mode"), mode).unwrap();
        }

        /// 一顆一直在生新「編譯器」的 cargo：兩個背景迴圈不停 fork 出 `sleep`（一個直接生、一個包一層子 shell），
        /// 每生一顆就把 pid 記進 `spawned.pids`。抓的是 cargo 在「拍行程快照」與「送訊號」之間又生出新行程的競態——
        /// 生出來的行程父親一死就被 init 收養，事後按父子關係再也追不到。
        fn install_spawning_cargo(&self) {
            let d = self.dir.display();
            let body = format!(
                "#!/bin/sh\necho $$ > '{d}/cargo.pid'\n\
                 ( while :; do /bin/sleep 300 >/dev/null 2>&1 & echo $! >> '{d}/spawned.pids'; /bin/sleep 0.02; done ) &\n\
                 ( while :; do ( /bin/sleep 302 >/dev/null 2>&1 & echo $! >> '{d}/spawned.pids'; wait ) & /bin/sleep 0.03; done ) &\n\
                 touch '{d}/cargo.ready'\n/bin/sleep 120\necho done > '{d}/cargo.done'\n"
            );
            let path = self.dir.join("real/cargo");
            std::fs::write(&path, body).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        /// `spawned.pids` 記下的所有 pid。
        fn spawned(&self) -> Vec<i32> {
            std::fs::read_to_string(self.dir.join("spawned.pids")).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect()
        }

        fn pid(&self, file: &str) -> Option<i32> {
            std::fs::read_to_string(self.dir.join(file)).ok()?.trim().parse().ok()
        }

        /// 這顆 pid 還活著嗎（`kill -0`）。
        fn alive(&self, file: &str) -> bool {
            self.pid(file).is_some_and(|p| unsafe { libc::kill(p, 0) } == 0)
        }

        /// 起一個 shim，放進**自己的 process group**（issue #151）並登記：Drop（含測試 panic）時整組終止。
        /// stdout／stderr 寫進沙盒裡的檔案（不是 pipe：假 cargo 的子行程握著 pipe 會讓讀取端等到它結束）。
        fn start_group(&self, cmd: &mut Command, stdin: bool) -> (std::process::Child, std::path::PathBuf, std::path::PathBuf) {
            use std::os::unix::process::CommandExt as _;
            let tag = crate::db::ulid();
            let (out, err) = (self.dir.join(format!("out-{tag}")), self.dir.join(format!("err-{tag}")));
            cmd.process_group(0)
                .stdout(std::fs::File::create(&out).unwrap())
                .stderr(std::fs::File::create(&err).unwrap())
                .stdin(if stdin { std::process::Stdio::piped() } else { std::process::Stdio::null() });
            let child = spawn_retrying(cmd);
            self.groups.lock().unwrap().push(child.id() as i32);
            (child, out, err)
        }

        /// 跑一個 shim 並保證**不留行程**（issue #151）：有時間上限（卡住就整組殺掉並報錯，不是永遠等下去）、
        /// 結束後斷言組內一個行程都不剩（`sleep`、等名額的迴圈、假編譯器都算）。
        fn run_group(&self, mut cmd: Command, stdin: Option<&[u8]>) -> (String, String, i32) {
            let (mut child, out_path, err_path) = self.start_group(&mut cmd, stdin.is_some());
            let pgid = child.id() as i32;
            if let Some(data) = stdin {
                child.stdin.take().unwrap().write_all(data).unwrap();
            }
            let started = std::time::Instant::now();
            let status = loop {
                if let Some(st) = child.try_wait().unwrap() {
                    break st;
                }
                if started.elapsed() > std::time::Duration::from_secs(120) {
                    unsafe { libc::killpg(pgid, libc::SIGKILL) };
                    panic!("shim 跑了 120 秒還沒結束（卡在等名額？）：{}", std::fs::read_to_string(&err_path).unwrap_or_default());
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            self.assert_group_gone(pgid);
            (
                std::fs::read_to_string(&out_path).unwrap_or_default(),
                std::fs::read_to_string(&err_path).unwrap_or_default(),
                status.code().unwrap_or(-1),
            )
        }

        /// 這個 process group 在幾秒內要一個行程都不剩；剩下的補殺並讓測試失敗（issue #151 的回歸）。
        fn assert_group_gone(&self, pgid: i32) {
            // 正常情況下一兩個 50ms 就空了；上限放寬到 40 秒是因為本機常常同時有很多人在編譯（行程表塞滿時 fork 都會失敗），
            // 只有真的留下行程的失敗路徑才會等滿（壓測 40 個並行 shim，最慢的要 7 秒才起來）。
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
            loop {
                let left = group_members(pgid);
                if left.is_empty() {
                    return;
                }
                if std::time::Instant::now() > deadline {
                    unsafe { libc::killpg(pgid, libc::SIGKILL) };
                    panic!("shim 結束後還留下行程（孤兒）：{left:?}");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        fn run(&self, env: &[(&str, &str)], args: &[&str]) -> (String, String, i32) {
            self.run_in(None, env, args)
        }

        /// 指定用哪個 shell 跑 shim（macOS 內建 `/bin/sh` 與 `/bin/bash` 是 3.2、另有 `/bin/dash`）；
        /// `None` 就照 shebang。
        fn run_in(&self, shell: Option<&str>, env: &[(&str, &str)], args: &[&str]) -> (String, String, i32) {
            let path = format!(
                "{}:{}:/usr/bin:/bin",
                self.dir.join("bin").display(),
                self.dir.join("real").display()
            );
            let mut cmd = self.command_in(shell, &path);
            cmd.args(args);
            for (k, v) in env {
                cmd.env(k, v);
            }
            self.run_group(cmd, None)
        }
    }

    /// 不吃 rustc 的子指令（`--version`、`metadata`）直接 exec 真的 cargo，完全不碰排程器（curl 都不叫）。
    #[test]
    fn a_light_subcommand_skips_the_scheduler_entirely() {
        let s = Sandbox::new();
        // 沒有安裝假 curl：如果 shim 誤判去呼叫排程器，這裡會因為找不到 curl 或連不上而看得出來。
        let log = s.dir.join("cargo.log");
        let (_, err, rc) = s.run(&[("AM_TEST_FAKE_CARGO_LOG", log.to_str().unwrap())], &["--version"]);
        assert_eq!(rc, 0, "{err}");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains("--version"), "{logged}");
        assert!(!err.contains("scheduler"), "{err}");
    }

    /// 沒有 bot token 也沒有 UI token 檔：直接跳過排程，不假裝有身分（issue #90 的「明講的 bypass」）。
    #[test]
    fn no_identity_available_bypasses_with_a_warning() {
        let s = Sandbox::new();
        let log = s.dir.join("cargo.log");
        let (_, err, rc) = s.run(&[("AM_TEST_FAKE_CARGO_LOG", log.to_str().unwrap())], &["build"]);
        assert_eq!(rc, 0, "{err}");
        assert!(err.contains("不經過排程器"), "{err}");
        assert!(std::fs::read_to_string(&log).unwrap().contains("build"));
    }

    /// daemon 連不上（curl 失敗）：不排程，直接跑，不是掛在那裡等。
    #[test]
    fn an_unreachable_daemon_bypasses_instead_of_hanging() {
        let s = Sandbox::new();
        s.install_fake_curl("exit 7\n"); // curl 的「連不上」退出碼
        let log = s.dir.join("cargo.log");
        let (_, err, rc) = s.run(
            &[("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_TEST_FAKE_CARGO_LOG", log.to_str().unwrap())],
            &["test", "-p", "agents-managerd"],
        );
        assert_eq!(rc, 0, "{err}");
        assert!(err.contains("連不上"), "{err}");
        assert!(std::fs::read_to_string(&log).unwrap().contains("agents-managerd"));
    }

    /// 名額滿了先等，daemon 說 granted 才跑；granted 帶的 `cargo_jobs` 要真的傳進 CARGO_BUILD_JOBS。
    #[test]
    fn it_waits_out_a_full_scheduler_then_runs_with_the_granted_job_count() {
        let s = Sandbox::new();
        let call_log = s.dir.join("curl-calls.log");
        s.install_fake_curl(&format!(
            r#"echo "$@" >> '{log}'
n=$(grep -c acquire '{log}' 2>/dev/null || echo 0)
case "$*" in
  *acquire*)
    if [ "$n" -le 1 ]; then printf '{{"granted":false,"active":2,"retry_after_secs":0}}'; else printf '{{"granted":true,"token":"tok-abc","cargo_jobs":3,"lease_ttl_secs":30}}'; fi
    ;;
  *renew*|*release*) printf '{{}}' ;;
esac
"#,
            log = call_log.display()
        ));
        let cargo_log = s.dir.join("cargo.log");
        let (_, err, rc) = s.run(
            &[("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap())],
            &["check", "-p", "agents-managerd"],
        );
        assert_eq!(rc, 0, "{err}");
        let logged = std::fs::read_to_string(&cargo_log).unwrap();
        assert!(logged.contains("CARGO_BUILD_JOBS=3"), "{logged}");
        assert!(logged.contains("agents-managerd"), "{logged}");
        let calls = std::fs::read_to_string(&call_log).unwrap();
        assert!(calls.matches("acquire").count() >= 2, "第一次滿了，第二次才拿到：{calls}");
        assert!(calls.contains("release"), "結束要放：{calls}");
    }

    /// issue #138：check／test／clippy 該轉到外部編譯主機、卻因為 pane 缺 `AM_DAEMON_EXE`／`AM_CONFIG_PATH`／
    /// `AM_DATA_DIR`（或 helper 不能執行）而退回本機時，要講出來，不能靜默——靜默的結果就是整批子 agent 的
    /// 編譯都塞在本機排隊，沒有人知道 #104 根本沒生效。build 這類本來就不 offload 的不吵。
    #[test]
    fn falling_back_to_local_for_a_missing_offload_variable_says_which_one() {
        let s = Sandbox::new();
        s.install_fake_curl(
            r#"case "$*" in
  *acquire*) printf '{"granted":true,"token":"tok-1","cargo_jobs":2,"lease_ttl_secs":30}' ;;
  *) printf '{}' ;;
esac
"#,
        );
        let cargo_log = s.dir.join("cargo.log");
        let log = cargo_log.to_str().unwrap();
        fn base<'a>(log: &'a str, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
            let mut env = vec![("AM_BOT_ID", "b1"), ("AM_HOOK_TOKEN", "tok"), ("AM_TEST_FAKE_CARGO_LOG", log)];
            env.extend_from_slice(extra);
            env
        }
        // 一個真的能執行的假 helper（回 125＝退回本機；不能用 `/bin/true`——macOS 沒有這個路徑）。
        let helper = s.dir.join("fake-helper");
        std::fs::write(&helper, "#!/bin/sh\nexit 125\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let helper_path = helper.to_str().unwrap();
        // 什麼都沒有：一次講清楚缺哪三個。
        let (_, err, rc) = s.run(&base(log, &[]), &["check", "-p", "agents-managerd"]);
        assert_eq!(rc, 0, "{err}");
        assert!(err.contains("外部編譯沒有啟用"), "{err}");
        for k in ["AM_DAEMON_EXE", "AM_CONFIG_PATH", "AM_DATA_DIR"] {
            assert!(err.contains(k), "要點名缺 {k}：{err}");
        }
        assert!(std::fs::read_to_string(&cargo_log).unwrap().contains("agents-managerd"), "照樣在本機跑");
        // 只缺一個：只點那一個。
        let (_, err, _) = s.run(&base(log, &[("AM_DAEMON_EXE", helper_path), ("AM_DATA_DIR", "/tmp")]), &["clippy"]);
        assert!(err.contains("AM_CONFIG_PATH") && !err.contains("AM_DAEMON_EXE") && !err.contains("AM_DATA_DIR"), "{err}");
        // helper 路徑在、但不能執行（binary 被換掉／搬走）：也算缺。
        let (_, err, _) = s.run(&base(log, &[("AM_DAEMON_EXE", "/nonexistent/agents-managerd"), ("AM_CONFIG_PATH", "/tmp/c.toml"), ("AM_DATA_DIR", "/tmp")]), &["test"]);
        assert!(err.contains("AM_DAEMON_EXE"), "{err}");
        // build／run 本來就不 offload：不吵。
        let (_, err, _) = s.run(&base(log, &[]), &["build", "--release"]);
        assert!(!err.contains("外部編譯"), "{err}");
        // 三個都齊：不提示（helper 會自己決定要不要轉；這裡假 helper 回 125＝退回本機）。
        let (_, err, rc) = s.run(
            &base(log, &[("AM_DAEMON_EXE", helper_path), ("AM_CONFIG_PATH", "/tmp/c.toml"), ("AM_DATA_DIR", "/tmp")]),
            &["check"],
        );
        assert_eq!(rc, 0, "{err}");
        assert!(!err.contains("外部編譯沒有啟用"), "齊全就不提示：{err}");
    }

    /// 這台機器上有的 shell（沒有的略過）。macOS 的 `/bin/sh`／`/bin/bash` 是 3.2——腳本改動要在那個版本也驗。
    fn shells() -> Vec<&'static str> {
        ["/bin/sh", "/bin/bash", "/bin/dash"].into_iter().filter(|p| std::path::Path::new(p).exists()).collect()
    }

    /// 租約守衛用的環境：有 bot 身分、暫存目錄指到沙盒裡（才看得出有沒有留下狀態目錄）。
    fn lease_env(s: &Sandbox) -> Vec<(&'static str, String)> {
        let tmp = s.dir.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        vec![("AM_BOT_ID", "b1".into()), ("AM_HOOK_TOKEN", "tok".into()), ("TMPDIR", tmp.display().to_string())]
    }

    fn as_refs<'a>(v: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
        v.iter().map(|(k, val)| (*k, val.as_str())).collect()
    }

    /// issue #128：名額是 TTL 租的，daemon 端停止續約超過 TTL 就把它收回、讓別人拿。持有者**活著但租約已失效**時
    /// 必須停止使用容量——以前續約迴圈把失敗全吞掉，前景的 cargo 照跑，於是 A 還在編、B 拿到同一個名額也開始編，
    /// `max_concurrent` 被突破。daemon **明確**說沒有這個名額（`not_found`）或 token 對不上（`token_mismatch`）：
    /// 名額已經不是我們的，整棵 cargo／rustc 行程樹一起停、退 75（可重試）、名額放掉、狀態目錄清乾淨。
    /// 每一種 shell 都驗（含 macOS 的 bash 3.2）。用虛擬時鐘（TTL 180 秒，第一次續約在 60 秒）。
    #[test]
    fn an_explicit_renew_refusal_stops_cargo_and_every_compiler_under_it() {
        let cases: Vec<(&str, &str)> = shells().into_iter().map(|sh| (sh, "notfound")).chain([("/bin/sh", "mismatch")]).collect();
        std::thread::scope(|scope| {
            for (sh, mode) in cases {
                scope.spawn(move || {
                    let s = Sandbox::new();
                    s.install_virtual_clock();
                    s.install_lease_curl(180);
                    s.install_slow_cargo(120);
                    s.set_renew_mode(mode);
                    let env = lease_env(&s);
                    let (_, err, rc) = s.run_in(Some(sh), &as_refs(&env), &["check", "-p", "agents-managerd"]);
                    let why = format!("{sh} {mode}: {err}");
                    assert_eq!(rc, 75, "被我們停掉的一律退 75（可重試）：{why}");
                    assert!(s.pid("cargo.pid").is_some() && s.pid("rustc.pid").is_some(), "cargo 得真的起來過，不然這條測試是空過：{why}");
                    assert!(err.contains("名額租約失效"), "要告訴使用者為什麼：{why}");
                    assert!(!s.dir.join("cargo.done").exists(), "cargo 不能跑完：{why}");
                    assert!(s.virtual_secs() < 90, "第一次續約（60 秒）就該停，不是等到 TTL：{}s {why}", s.virtual_secs());
                    assert!(!s.alive("cargo.pid"), "cargo 還活著：{why}");
                    assert!(!s.alive("rustc.pid"), "cargo 底下的編譯器還活著（孤兒）：{why}");
                    let calls = std::fs::read_to_string(s.dir.join("curl.log")).unwrap();
                    assert!(calls.contains("/release"), "名額要放掉：{calls}");
                    let leftovers: Vec<_> = std::fs::read_dir(s.dir.join("tmp")).unwrap().flatten().collect();
                    assert!(leftovers.is_empty(), "狀態目錄要清掉：{leftovers:?}");
                });
            }
        });
    }

    /// 停的是**整棵**行程樹，而且是「凍住再殺」：cargo 一直在生新的 rustc（兩個迴圈不停 fork），如果只是拍一張快照、
    /// 對快照裡的 pid 送訊號，快照之後才生出來的行程父親一死就被 init 收養、再也追不到——一顆孤兒編譯器。
    /// 先 `SIGSTOP` 凍住、重拍到不再長新的，才 `TERM`。每一種 shell 都驗；活著的一個都不能剩。
    #[test]
    fn a_cargo_that_keeps_spawning_compilers_is_stopped_without_orphans() {
        std::thread::scope(|scope| {
            for sh in shells() {
                scope.spawn(move || {
                    let s = Sandbox::new();
                    s.install_virtual_clock();
                    s.install_lease_curl(180);
                    s.install_spawning_cargo();
                    s.set_renew_mode("notfound");
                    let env = lease_env(&s);
                    let (_, err, rc) = s.run_in(Some(sh), &as_refs(&env), &["build"]);
                    assert_eq!(rc, 75, "{sh}: {err}");
                    let spawned = s.spawned();
                    assert!(spawned.len() >= 2, "{sh}: 假 cargo 應該已經生出一批行程：{spawned:?}");
                    let alive: Vec<i32> = spawned.iter().copied().filter(|p| unsafe { libc::kill(*p, 0) } == 0).collect();
                    assert!(alive.is_empty(), "{sh}: 孤兒編譯器還活著：{alive:?}（共生出 {}）", spawned.len());
                    assert!(!s.alive("cargo.pid"), "{sh}: cargo 還活著");
                });
            }
        });
    }

    /// 續約**一直**失敗（daemon 連不上）：不能等到 daemon 端的到期時間之後才動手——那時 B 可能已經拿到同一個名額。
    /// TTL 180 秒：在它到期**之前**就要把 A 停掉（續約在 60、120 秒失敗，第二次失敗時離到期只剩一分鐘、
    /// 已經趕不及再試一次，就現在停），而且行程樹一起停。
    #[test]
    fn a_renew_that_keeps_failing_stops_cargo_before_the_lease_can_be_reclaimed() {
        let s = Sandbox::new();
        s.install_virtual_clock();
        s.install_lease_curl(180);
        s.install_slow_cargo(120);
        s.set_renew_mode("down");
        let env = lease_env(&s);
        let (_, err, rc) = s.run(&as_refs(&env), &["test"]);
        assert_eq!(rc, 75, "{err}");
        assert!(s.pid("cargo.pid").is_some() && s.pid("rustc.pid").is_some(), "cargo 得真的起來過，不然這條測試是空過：{err}");
        let at = s.virtual_secs();
        assert!(at < 180, "daemon 在 180 秒收回名額——A 必須在那之前就停：{at}s\n{err}");
        assert!(at >= 120, "第一次失敗不是死刑（還有機會再試一次）：{at}s");
        assert!(!s.dir.join("cargo.done").exists());
        assert!(!s.alive("cargo.pid") && !s.alive("rustc.pid"), "cargo 與它的編譯器都要停");
    }

    /// 短暫的一次續約失敗、在到期之前下一次就成功：建置不能被誤殺；而且續約真的把租約往後延——
    /// 這一趟的虛擬時間走過好幾個 TTL，照樣跑完、退 0。
    #[test]
    fn one_failed_renew_before_expiry_does_not_kill_a_build_that_outlives_the_ttl() {
        let s = Sandbox::new();
        s.install_virtual_clock();
        s.install_lease_curl(180);
        s.install_slow_cargo(3);
        s.set_renew_mode("once_down");
        let env = lease_env(&s);
        let (_, err, rc) = s.run(&as_refs(&env), &["check"]);
        assert_eq!(rc, 0, "{err}");
        assert!(!err.contains("名額租約失效"), "{err}");
        assert!(s.dir.join("cargo.done").exists(), "應該正常跑完");
        assert!(s.virtual_secs() > 3 * 180, "虛擬時間要走過好幾個 TTL 才算數：{}s", s.virtual_secs());
    }

    /// 交替失敗（一次連不上、一次成功……）的租約是健康的：每次成功都把 deadline 往後延，所以永遠撐得到下一次成功。
    /// deadline 沒有隨續約成功往後延的話，第二次失敗就會照最早那個 deadline 判死。
    #[test]
    fn alternating_renew_failures_never_kill_a_lease_that_keeps_getting_renewed() {
        let s = Sandbox::new();
        s.install_virtual_clock();
        s.install_lease_curl(180);
        s.install_slow_cargo(3);
        s.set_renew_mode("alternate");
        let env = lease_env(&s);
        let (_, err, rc) = s.run(&as_refs(&env), &["test"]);
        assert_eq!(rc, 0, "{err}");
        assert!(!err.contains("名額租約失效"), "{err}");
        assert!(s.dir.join("cargo.done").exists(), "應該正常跑完");
        assert!(s.virtual_secs() > 3 * 180, "{}s", s.virtual_secs());
    }

    /// issue #151：呼叫 shim 的那一端（agent 的 shell、被砍的測試行程）不在了，等名額的迴圈不能變成永遠在等的孤兒。
    /// 假的排程器永遠說額滿（`retry_after_secs=0`，等同忙等）；shim 的呼叫者是一個馬上結束的 `sh -c`。
    /// 以前這顆 shim 會一直轉下去，一輪全套就留下一批，多顆 agent 反覆跑就把機器的行程表塞滿。
    #[test]
    fn a_shim_whose_caller_is_gone_stops_waiting_for_a_slot() {
        let s = Sandbox::new();
        s.install_fake_curl(
            r#"case "$*" in
  *acquire*) printf '{"granted":false,"active":2,"retry_after_secs":0}' ;;
  *) printf '{}' ;;
esac"#,
        );
        let env = lease_env(&s);
        // 外層是 `sh -c`（不是 shim 本身），所以不走 `s.command()`——但同樣清掉呼叫端所有的 `AM_*`。
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("'{}' build & echo $! > '{}'", s.dir.join("bin/cargo").display(), s.dir.join("shim.pid").display()))
            .env("PATH", format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display()))
            .env("HOME", s.dir.join("fake-home"));
        for (key, _) in std::env::vars() {
            if key.starts_with("AM_") {
                cmd.env_remove(key);
            }
        }
        for (k, v) in &env {
            cmd.env(k, v);
        }
        // 外層 `sh -c` 起完 shim 就結束：shim 的 `$PPID` 指到一個已經不在的行程。
        let (mut outer, _, err_path) = s.start_group(&mut cmd, false);
        let group = outer.id() as i32;
        assert!(outer.wait().unwrap().success());
        // 整組（含 shim）要自己收掉；卡住的話補殺並讓測試失敗，訊息附上 shim 講過的話。
        let waited = std::time::Instant::now();
        while !group_members(group).is_empty() && waited.elapsed() < std::time::Duration::from_secs(40) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let left = group_members(group);
        unsafe { libc::killpg(group, libc::SIGKILL) };
        let said = std::fs::read_to_string(&err_path).unwrap_or_default();
        assert!(left.is_empty(), "呼叫端不在了，shim 還在等名額：{left:?}\n{said}");
        // 光看「行程消失了」不夠（機器上可能有別的東西在清孤兒）：shim 要**自己**講一句並退出，才是它自己發現的。
        assert!(said.contains("呼叫端已經結束"), "shim 要自己發現呼叫端不在了，而不是被別人殺掉：{said}");
    }

    /// issue #151（也是 #128 的延伸）：shim 自己被 `SIGKILL`（`trap` 沒機會跑、名額沒人放）時，背景的續約迴圈
    /// 不能永遠續下去把名額佔到重開機。cargo 還活著就繼續續約（它還在用容量，租約不能先掉）；
    /// cargo 也沒了，就把名額放掉、收掉狀態目錄、自己結束。
    #[test]
    fn a_renew_loop_whose_shim_was_killed_stops_once_cargo_is_gone_and_releases_the_slot() {
        let s = Sandbox::new();
        s.install_virtual_clock();
        s.install_lease_curl(180);
        s.install_slow_cargo(120);
        let env = lease_env(&s);
        let mut cmd = s.command(&format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display()));
        cmd.args(["check"]);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let (mut shim, _, _) = s.start_group(&mut cmd, false);
        let group = shim.id() as i32;
        let up = std::time::Instant::now();
        while !s.dir.join("cargo.ready").exists() {
            assert!(up.elapsed() < std::time::Duration::from_secs(30), "cargo 沒起來");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // shim 被 SIGKILL：續約迴圈與 cargo 都成了孤兒。
        unsafe { libc::kill(shim.id() as i32, libc::SIGKILL) };
        let _ = shim.wait();
        let renews = || std::fs::read_to_string(s.dir.join("curl.log")).unwrap().matches("/renew").count();
        let before = renews();
        std::thread::sleep(std::time::Duration::from_millis(600));
        assert!(renews() > before, "cargo 還活著：續約要繼續（它還在用容量）");
        assert!(!group_members(group).is_empty(), "續約迴圈還在");

        // cargo 也沒了（連它前景那顆 `sleep 120` 一起收掉）：迴圈放掉名額、收掉狀態目錄、自己結束。
        for f in ["cargo.pid", "rustc.pid"] {
            if let Some(p) = s.pid(f) {
                unsafe { libc::kill(p, libc::SIGKILL) };
            }
        }
        for (pid, cmd) in group_members(group) {
            if cmd.contains("sleep 120") {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        s.assert_group_gone(group);
        let calls = std::fs::read_to_string(s.dir.join("curl.log")).unwrap();
        assert!(calls.contains("/release"), "名額要放掉：{calls}");
        let leftovers: Vec<_> = std::fs::read_dir(s.dir.join("tmp")).unwrap().flatten().collect();
        assert!(leftovers.is_empty(), "狀態目錄要清掉：{leftovers:?}");
    }

    /// 前景執行的語意不能因為多了守衛而變：stdin 照樣接得到 cargo（`cargo run` 起來的程式要讀輸入）。
    #[test]
    fn the_guard_keeps_stdin_flowing_to_cargo() {
        let s = Sandbox::new();
        s.install_lease_curl(30);
        let sink = s.dir.join("stdin.got");
        std::fs::write(s.dir.join("real/cargo"), format!("#!/bin/sh\ncat > '{}'\nexit 0\n", sink.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(s.dir.join("real/cargo"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = lease_env(&s);
        let mut cmd = s.command(&format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display()));
        cmd.args(["run"]);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let (_, err, rc) = s.run_group(cmd, Some(b"hello-from-the-terminal"));
        assert_eq!(rc, 0, "{err}");
        assert_eq!(std::fs::read_to_string(&sink).unwrap(), "hello-from-the-terminal");
    }

    /// issue #128 的整合驗收：**真的** router＋**真的** curl＋**真的** shim＋真時鐘（`max_concurrent=1`、TTL 21 秒：續約在 7、14 秒，
    /// 第二次失敗時離到期還有 7 秒、趕不及再試就停，餘裕夠大，機器忙也不會誤判）。
    /// A 拿到唯一的名額、開始「編譯」；daemon 中斷（server 整個關掉）超過 TTL。B 在同一段時間裡一直在問名額。
    /// 不變式：**任何時刻，活著的編譯器不超過設定的名額**——B 一拿到名額的那一刻，A 的 cargo 與它底下的
    /// 編譯器必須已經死了（以前續約失敗被吞掉、A 照跑，daemon 到期收回名額後 B 拿到、兩個同時編）。
    /// 之後 daemon 回來（新的 App 開同一個資料庫，就像重啟）：名額表還在、B 的持有正常、沒有卡死。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_daemon_outage_longer_than_the_lease_never_leaves_two_compilers_alive() {
        use crate::build_scheduler::{acquire, Acquired};
        use std::time::{Duration, Instant};

        let env = crate::testing::env().await;
        env.app
            .cfg
            .update(|c| {
                c.build.max_concurrent = 1;
                c.build.lease_ttl_secs = 21;
                Ok(())
            })
            .await
            .unwrap();
        let bind = || std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listener = bind();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let serve = |app: std::sync::Arc<crate::state::App>, listener: std::net::TcpListener| {
            let router = crate::api::router(app);
            tokio::spawn(async move {
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                let _ = axum::serve(l, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
            })
        };
        let server = serve(env.app.clone(), listener);

        // 沙盒：真的 shim、假 cargo（25 秒，會生一顆 300 秒的「rustc」）、真的 curl（PATH 上 /usr/bin/curl）。
        let s = Sandbox::new();
        s.install_slow_cargo(25);
        let home = s.dir.join("fake-home/.config/agents-manager");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("ui-token"), "test-token").unwrap();
        let mut cmd = s.command(&format!("{}:{}:/usr/bin:/bin", s.dir.join("bin").display(), s.dir.join("real").display()));
        cmd.env("AM_PORT", port.to_string())
            .env("TMPDIR", s.dir.join("tmp"))
            .args(["check", "-p", "agents-managerd"]);
        std::fs::create_dir_all(s.dir.join("tmp")).unwrap();
        let (mut a, _, shim_err) = s.start_group(&mut cmd, false);
        let a_group = a.id() as i32;

        // A 拿到名額、cargo 真的起來了。
        let up = Instant::now();
        while crate::build_scheduler::status(&env.app).await.unwrap()["active"] != 1 || s.pid("rustc.pid").is_none() {
            assert!(up.elapsed() < Duration::from_secs(30), "A 沒拿到名額：{}", std::fs::read_to_string(&shim_err).unwrap_or_default());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(s.alive("cargo.pid") && s.alive("rustc.pid"));

        // daemon 中斷（server 整個關掉）。B 從這一刻起每 100ms 問一次名額，直到拿到。
        let outage = Instant::now();
        server.abort();
        let mut a_dead_at: Option<Duration> = None;
        let granted_at = loop {
            assert!(outage.elapsed() < Duration::from_secs(60), "B 一直拿不到名額（名額表卡死了？）");
            if a_dead_at.is_none() && a.try_wait().unwrap().is_some() && !s.alive("cargo.pid") && !s.alive("rustc.pid") {
                a_dead_at = Some(outage.elapsed());
            }
            if let Acquired::Granted { .. } = acquire(&env.app, "B:1", None, "check", "local").await.unwrap() {
                break outage.elapsed();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let dead = a_dead_at.unwrap_or_else(|| panic!("B 拿到名額（{granted_at:?}）的時候，A 的 cargo／rustc 還活著——兩個編譯同時在跑，max_concurrent=1 被突破"));
        assert!(dead <= granted_at, "A 要先死（{dead:?}）B 才拿得到名額（{granted_at:?}）");
        assert_eq!(a.wait().unwrap().code(), Some(75), "被停掉的 A 退 75（可重試）：{}", std::fs::read_to_string(&shim_err).unwrap_or_default());
        assert!(!s.dir.join("cargo.done").exists(), "A 不能跑完");
        s.assert_group_gone(a_group);

        // daemon 回來（新的 App 開同一個資料庫）：B 持有的名額正常，名額表沒有卡死。
        let app2 = crate::testing::restart_app(&env).await;
        let server2 = serve(app2.clone(), bind_on(port));
        let st = crate::build_scheduler::status(&app2).await.unwrap();
        assert_eq!(st["active"], 1, "只有 B 一個：{st}");
        server2.abort();
    }

    /// 在指定的 port 上重開一個 listener（daemon 重啟後回到同一個位址）。
    fn bind_on(port: u16) -> std::net::TcpListener {
        let l = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        l.set_nonblocking(true).unwrap();
        l
    }

    /// PATH 上還掛著**別顆 bot 的同一支 shim**（祖先 pane 繼承下來的，2026-09-18 實測有 6 個）：
    /// 真 cargo 要往後找，不能把另一份 shim 當成真 cargo——那會 shim → shim 一層層各拿一個名額，
    /// `max_concurrent` 被自己的外層佔滿，最內層永遠等不到（w168:p91 卡死 17 分鐘）。
    #[test]
    fn another_bots_shim_on_path_is_not_mistaken_for_the_real_cargo() {
        let s = Sandbox::new();
        // 另一顆 bot 的 bin 目錄，插在自己的 bin 與真 cargo 之間。
        let other = s.dir.join("other-bot");
        super::install_local(&other).unwrap();
        let call_log = s.dir.join("curl-calls.log");
        s.install_fake_curl(&format!(
            r#"echo "$@" >> '{log}'
case "$*" in
  *acquire*) printf '{{"granted":true,"token":"tok-1","cargo_jobs":2,"lease_ttl_secs":30}}' ;;
  *) printf '{{}}' ;;
esac
"#,
            log = call_log.display()
        ));
        let cargo_log = s.dir.join("cargo.log");
        let path = format!(
            "{}:{}:{}:/usr/bin:/bin",
            s.dir.join("bin").display(),
            other.join("bin").display(),
            s.dir.join("real").display()
        );
        let mut cmd = s.command(&path);
        cmd.env("AM_BOT_ID", "b1").env("AM_HOOK_TOKEN", "tok").env("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap());
        cmd.args(["check", "-p", "agents-managerd"]);
        let (_, err, rc) = s.run_group(cmd, None);
        assert_eq!(rc, 0, "{err}");
        // 真 cargo 真的跑到了（不是卡在等名額），而且整趟只拿一個名額。
        assert!(std::fs::read_to_string(&cargo_log).unwrap().contains("agents-managerd"), "{err}");
        let calls = std::fs::read_to_string(&call_log).unwrap();
        assert_eq!(calls.matches("acquire").count(), 1, "一層一個名額就是死結：{calls}");
    }

    /// 2026-09-18 兩次死鎖的可重現版：PATH 上兩層 shim（其中一層是**舊版**、沒有
    /// `AM_SHIM_MARKER`，換版期間就是這樣混著），排程器只有兩個名額而且**不會**再多給。
    ///
    /// 舊的行為：每一層各拿一個名額，第三層永遠等——`cargo` 一次都沒跑到。現在只有最外層排隊，
    /// 真 cargo 一定跑得到，而且整趟只吃一個名額。
    #[test]
    fn two_layers_of_shims_with_only_two_slots_do_not_deadlock() {
        let s = Sandbox::new();
        // 第二層：別顆 bot 的 bin，而且是**舊版** shim（沒有 marker），只認得出檔頭的話會漏掉它。
        let other = s.dir.join("bots").join("OTHER").join("bin");
        std::fs::create_dir_all(&other).unwrap();
        let old_shim = super::SHIM_SH.replace("AM_SHIM_MARKER", "(舊版沒有這一行)");
        std::fs::write(other.join("cargo"), &old_shim).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(other.join("cargo"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // 名額上限 2，而且拿滿就不再給——真的死鎖時這個測試會停在這裡（fake curl 不會 sleep，
        // shim 的 retry_after_secs=0，所以是一個忙等的迴圈，不是 10 分鐘的假等待）。
        let call_log = s.dir.join("curl-calls.log");
        s.install_fake_curl(&format!(
            r#"echo "$@" >> '{log}'
case "$*" in
  *acquire*)
    n=$(grep -c acquire '{log}')
    if [ "$n" -le 2 ]; then printf '{{"granted":true,"token":"tok-$n","cargo_jobs":2,"lease_ttl_secs":30}}';
    else printf '{{"granted":false,"active":2,"retry_after_secs":0}}'; fi
    ;;
  *) printf '{{}}' ;;
esac
"#,
            log = call_log.display()
        ));
        let cargo_log = s.dir.join("cargo.log");
        let path = format!(
            "{}:{}:{}:/usr/bin:/bin",
            s.dir.join("bin").display(),
            other.display(),
            s.dir.join("real").display()
        );
        let mut cmd = s.command(&path);
        cmd.env("AM_BOT_ID", "b1").env("AM_HOOK_TOKEN", "tok").env("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap());
        cmd.args(["build", "--release"]);
        let (_, err, rc) = s.run_group(cmd, None);
        assert_eq!(rc, 0, "{err}");
        assert!(std::fs::read_to_string(&cargo_log).unwrap().contains("--release"), "真 cargo 沒跑到：{err}");
        let calls = std::fs::read_to_string(&call_log).unwrap();
        assert_eq!(calls.matches("acquire").count(), 1, "一層一個名額就是死結：{calls}");
    }

    /// build script／xtask 在拿著名額的 cargo 裡再叫一次 cargo：直接跑，不再排一次
    /// （內層等的名額要等外層結束才空，而外層在等內層）。
    #[test]
    fn a_nested_cargo_inside_a_held_slot_does_not_queue_again() {
        let s = Sandbox::new();
        let call_log = s.dir.join("curl-calls.log");
        s.install_fake_curl(&format!("echo \"$@\" >> '{log}'\nprintf '{{}}'\n", log = call_log.display()));
        let cargo_log = s.dir.join("cargo.log");
        let (_, err, rc) = s.run(
            &[
                ("AM_BOT_ID", "b1"),
                ("AM_HOOK_TOKEN", "tok"),
                ("AM_BUILD_SLOT_HELD", "1"),
                ("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap()),
            ],
            &["build"],
        );
        assert_eq!(rc, 0, "{err}");
        assert!(std::fs::read_to_string(&cargo_log).unwrap().contains("build"), "{err}");
        assert!(!s.dir.join("curl-calls.log").exists() || !std::fs::read_to_string(&call_log).unwrap().contains("acquire"), "不該再排一次");
    }

    /// 真的 cargo 跑失敗：shim 仍然要放掉名額（不能因為建置失敗就卡住別人），並把 cargo 的結束碼原樣帶出去。
    #[test]
    fn a_failing_build_still_releases_its_slot_and_keeps_the_exit_code() {
        let s = Sandbox::new();
        let call_log = s.dir.join("curl-calls.log");
        s.install_fake_curl(&format!(
            r#"echo "$@" >> '{log}'
case "$*" in
  *acquire*) printf '{{"granted":true,"token":"tok-xyz","cargo_jobs":2,"lease_ttl_secs":30}}' ;;
  *) printf '{{}}' ;;
esac
"#,
            log = call_log.display()
        ));
        let cargo_log = s.dir.join("cargo.log");
        let (_, _, rc) = s.run(
            &[
                ("AM_BOT_ID", "b1"),
                ("AM_HOOK_TOKEN", "tok"),
                ("AM_TEST_FAKE_CARGO_LOG", cargo_log.to_str().unwrap()),
                ("AM_TEST_FAKE_CARGO_EXIT", "101"),
            ],
            &["build"],
        );
        assert_eq!(rc, 101);
        let calls = std::fs::read_to_string(&call_log).unwrap();
        assert!(calls.contains("release"), "失敗也要放：{calls}");
    }
}
