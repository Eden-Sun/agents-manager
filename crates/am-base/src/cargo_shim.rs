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
# shim, else `$CARGO_HOME/bin/cargo`, else `$HOME/.cargo/bin/cargo`.
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
    # PATH 找完再試 rustup 的安裝位置（issue #731）：daemon 由 daemon-start.py 以固定的最小 PATH 起，
    # 沒有 `~/.cargo/bin`（rustup 只改 shell profile），bot pane 繼承它，PATH 上就只剩 shim 自己。
    # 接在同一份清單後面，上面那幾道「是不是 shim」的防呆照樣套用。
    {
        printf '%s\n' "$PATH" | tr ':' '\n'
        [ -z "${CARGO_HOME:-}" ] || printf '%s\n' "$CARGO_HOME/bin"
        [ -z "${HOME:-}" ] || printf '%s\n' "$HOME/.cargo/bin"
    } | {
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

# cargo 的命令列是 `cargo [+toolchain] [全域旗標…] <子指令> …`：第一個參數不一定是子指令（issue #195）——`cargo +nightly build`、`cargo -q build`、
# `cargo --locked test`、`cargo -Z unstable-options build` 的第一個參數都不是。以前只看 `$1`，這些一律被當成「輕量子指令」直接 exec，
# 不問排程器、不佔名額、也不轉外部編譯。這裡找出真正的子指令：跳過 `+toolchain` 與全域旗標（`-Z`／`-C`／`--config`／`--color`／`--explain` 連它的值一起跳過）。
# 找不到子指令（`cargo`、`cargo --version`、`cargo -h`）就印空字串。
am_cargo_subcommand() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            +*) ;;
            -Z | -C | --config | --color | --explain) [ "$#" -lt 2 ] || shift ;;
            -*) ;;
            *)
                printf '%s' "$1"
                return 0
                ;;
        esac
        shift
    done
    return 0
}

# `build`/`check`/`test`/`clippy`/… multiply rustc processes;`metadata`/`tree`/`fmt`/`fetch`/… 這些**已知不編譯**的才放行。
# 反過來寫（不在輕量清單裡的都當成會編譯，issue #195）：`.cargo/config.toml` 的 alias（本 repo 的 `cargo dev` 展開成 `run`）由 cargo 自己展開、
# 自訂子指令（`cargo nextest`、`cargo xtask`）也可能編譯——認不得就當成 heavy，寧可排一下隊，不要悄悄繞過排程器。
am_cargo_is_heavy() {
    case "$1" in
        '' | version | help | metadata | tree | fmt | fetch | locate-project | pkgid | read-manifest | verify-project | search | login | logout | owner | yank | new | init | add | remove | rm | update | generate-lockfile | vendor | config | report | clean | uninstall) return 1 ;;
        *) return 0 ;;
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

# 會跑 libtest 測試的子指令（issue #813）：名額本來就罩住整個 test run（cargo 退出才放），但 `CARGO_BUILD_JOBS` 只限 rustc，
# 測試執行緒照 libtest 預設開到核心數——32 核上每支 test binary 80 個執行緒、吃 8 核以上，6 個名額加起來遠超過整台機器。
am_cargo_runs_tests() {
    case "$1" in
        t | test) return 0 ;;
        *) return 1 ;;
    esac
}

# 外部編譯（`remote-cargo` helper）需要的兩樣東西；缺哪個就回哪個的名字，空字串＝齊了。
# `AM_DAEMON_EXE` 指到的檔案不能執行（binary 被換掉／搬走）也算缺。
# `AM_DATA_DIR` 不在裡面（issue #417）：`scripts/check.sh` 為了不讓測試吃到正式資料目錄會清掉它，
# 當成前提的話每一次 check.sh 都退回本機排隊；沒有就不帶 `--data-dir`，helper 從設定檔推。
am_remote_cargo_missing() {
    _miss=""
    { [ -n "${AM_DAEMON_EXE:-}" ] && [ -x "$AM_DAEMON_EXE" ]; } || _miss="$_miss AM_DAEMON_EXE"
    [ -n "${AM_CONFIG_PATH:-}" ] || _miss="$_miss AM_CONFIG_PATH"
    printf '%s' "${_miss# }"
}

# `http://127.0.0.1:$AM_PORT` 的埠。**本機** bot 的 pane 一定有 `AM_PORT`（daemon 注入）；人工 host shell 沒有，
# 用文件寫的預設埠（SPEC：daemon 在 127.0.0.1:7788）。
#
# 有 bot 身分（`AM_BOT_ID`／`AM_BOT_TOKEN`）卻沒有 `AM_PORT` 的 pane 不能猜預設值（issue #153）：**遠端主機**上的 bot
# 就是這樣——遠端沒有 daemon、也不開反向埠（SPEC §11.4），127.0.0.1 是那台機器自己，猜 7788 打到的是不知道什麼東西
# （那台剛好也跑一份 agents-manager 的話，會把名額要求送給別顆 daemon）。回失敗，由呼叫端明講並直接跑。
am_build_port() {
    if [ -n "${AM_PORT:-}" ]; then
        printf '%s' "$AM_PORT"
        return 0
    fi
    if [ -n "${AM_BOT_ID:-}" ] || [ -n "${AM_BOT_TOKEN:-}" ] || [ -n "${AM_HOOK_TOKEN:-}" ]; then
        return 1
    fi
    printf '7788'
}

# 認證：bot 用自己的 API token（hook disabled 時也注入；舊 pane fallback 到同值的 hook token），人工 host shell 用一般 UI token（讀
# `~/.config/agents-manager/ui-token`，daemon 的預設位置）。兩個都沒有就回傳空字串，呼叫端看到空字串
# 就該直接跳過排程——沒有身分，daemon 也不可能認得這個名額。
am_build_auth_header() {
    if [ -n "${AM_BOT_ID:-}" ] || [ -n "${AM_BOT_TOKEN:-}" ] || [ -n "${AM_HOOK_TOKEN:-}" ]; then
        _bot_token=${AM_BOT_TOKEN:-${AM_HOOK_TOKEN:-}}
        [ -n "${AM_BOT_ID:-}" ] && [ -n "$_bot_token" ] || return 1
        printf 'X-AM-Bot-Token: %s' "$_bot_token"
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

# 受管的 bot pane：本機 bot（daemon 注入 `AM_BOT_ID`、bot token、`AM_PORT`）。這種 pane 的 cargo 不能因為排程器出問題就悄悄變成
# 沒有名額的 cargo（issue #128：daemon 重啟／升級／DB 出問題的瞬間，所有 bot 同時開編就繞過了 `max_concurrent`，
# 而那正是最需要保護本機的時候）。沒有 bot 身分的人工 host shell 才有「排程器出問題就直接跑」的隱式 bypass；bot 要繞過得明講
# （`AM_CARGO_BYPASS_SCHEDULER=1`），不是由連線錯誤自動取得。遠端 bot pane（沒有 `AM_PORT`）不是：那台機器的編譯本來就不受這台 daemon 管。
am_is_managed() {
    { [ -n "${AM_BOT_ID:-}" ] || [ -n "${AM_BOT_TOKEN:-}" ] || [ -n "${AM_HOOK_TOKEN:-}" ]; } && [ -n "${AM_PORT:-}" ]
}

# 排程器用不了、而且這一輪不會自己好（沒有 curl、建不出暫存目錄）：受管的 bot 不跑（exit 75，可重試）；人工 shell 印一行後回 0，
# 呼叫端接著 `exec` 真的 cargo。
am_scheduler_unusable() {
    if am_is_managed; then
        printf 'agents-manager: %s；受管的 bot 不會在沒有名額的情況下跑 cargo（會突破 max_concurrent，issue #128）。要明確繞過就 AM_CARGO_BYPASS_SCHEDULER=1 再跑一次\n' "$1" >&2
        exit 75
    fi
    printf 'agents-manager: %s，這次不排程，直接跑\n' "$1" >&2
    return 0
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
    if [ "$_kt_ppid" != "$$" ]; then
        # 不是 shim 的直接子行程。shim 還活著＝那個 pid 已經退出、被別的行程用掉了，不要亂殺。
        # shim 已經死了（被單獨 SIGKILL，issue #183）＝cargo 被 init 收養、ppid 變成 1，這個檢查永遠不成立：
        # 改認它是不是還在 shim 那一組（`_pgid`，shim 啟動時記下的 process group；重用這個 pid 的行程不會在這一組）。
        ! kill -0 "$$" 2>/dev/null || return 0
        [ -n "${_pgid:-}" ] && [ "$(ps -o pgid= -p "$1" 2>/dev/null | tr -d ' ')" = "$_pgid" ] || return 0
    fi
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
    # shim 已經被 SIGKILL：沒有人會來 `wait` 這個守衛、放名額、收狀態目錄（前景那邊才做這些）——自己收。
    if ! kill -0 "$_lw_shim" 2>/dev/null; then
        curl -s -m 5 -X POST "${_url}/release" --data-urlencode "holder=${_holder}" --data-urlencode "token=${_token}" >/dev/null 2>&1
        rm -rf "$_state" 2>/dev/null
    fi
    return 0
}

# 守衛的睡覺：背景 `sleep N &` 加 `wait`（issue #151、#189、#396）——**不是前景 `sleep`**：
# POSIX 的 trap 對正在跑的前景指令是**延後**執行的（要等那個前景指令跑完，shell 才回頭處理 trap），
# 所以前景 `sleep N` 收到 TERM 不會提早結束，反而要睡好睡滿；同一支 shim 的 stdout／stderr 又被這個背景守衛
# 原封不動繼承著，`Command::output()`（daemon 或任何呼叫端）要等**所有**繼承這兩支管線的行程都關閉它們才會返回——
# 一顆睡好睡滿才退出的守衛，就等於讓呼叫端也多等那一整段（#396：整樹高負載下，一支立刻退出的 `cargo check`
# 卻要 10 秒才真的收工，正是分成前景大段睡覺踩出來的）。
#
# 背景 `sleep &` + `wait` 沒有這個問題：`wait` 在等一顆背景工作時，POSIX shell 會**立刻**回應 trap（不像前景指令
# 要等它跑完），trap 直接 `kill -KILL` 那顆背景 sleep（KILL 送到就是送到，不能被接住或延後），對方馬上結束、
# 管線馬上關掉。但 TERM 本身還是可能在極端負載下掉（fork 與記下 pid 之間的縫、或 shell 版本本身的競態，
# 三種殼實測都會漏、稱不上罕見）：那個縫用 `_ls_gap` 記下來，縫過了照樣補殺。
#
# **加一層分段當安全網**：`AM_LEASE_SLEEP_CHUNK`（預設 10 秒）把一次 `am_lease_sleep` 拆成好幾段背景 sleep，
# 每段之間看一次 `$_state` 還在不在——TERM 真的整個漏光時（`_lw_term` 都沒設到），最壞也只會多活一段，
# 不會像最早的版本那樣抱著孤兒 `sleep` 活到天荒地老。這一層本身**不影響**正常（收到 TERM）路徑的速度：
# 常態下 `wait` 一收到訊號就交出控制權，不需要等到某一段睡滿。
AM_LEASE_SLEEP_CHUNK=${AM_LEASE_SLEEP_CHUNK:-10}

am_lease_sleep() {
    _ls_left=$1
    while [ "$_ls_left" -gt 0 ]; do
        _ls_step=$_ls_left
        [ "$_ls_step" -le "$AM_LEASE_SLEEP_CHUNK" ] || _ls_step=$AM_LEASE_SLEEP_CHUNK
        _ls_pid=""
        _ls_gap=1
        sleep "$_ls_step" &
        _ls_pid=$!
        _ls_gap=""
        if [ -n "$_lw_term" ]; then
            kill -KILL "$_ls_pid" 2>/dev/null
            exit 0
        fi
        wait "$_ls_pid"
        _ls_pid=""
        _ls_left=$((_ls_left - _ls_step))
        [ -d "$_state" ] || exit 0
    done
}

# 背景執行：續約，並判斷租約還在不在。
#
# * daemon 明確說沒有這個名額（`not_found`）或 token 對不上（`token_mismatch`）：名額已經不是我們的，立刻停。
# * 其他一切失敗（連不上、逾時、5xx、看不懂的回應）：先當暫時的，續約還有機會就繼續試；
#   但**不能等到 daemon 的到期時間之後才動手**——那時別人可能已經拿到同一個名額。所以估一個保守的
#   deadline（＝續約成功那個請求**送出**的時間 ＋ TTL，daemon 記的到期一定不早於它），
#   下一次重試（失敗之後隔 `_lw_retry`＝5 秒、最久再加 curl 逾時）會落在 deadline 之後就現在停。
# * 讀不到時間（`date +%s` 壞掉）：沒有依據可以等，第一次失敗就停。
am_lease_watch() {
    _lw_shim=$$
    trap ':' HUP
    _ls_pid=""
    _ls_gap=""
    _lw_term=""
    trap 'if [ -n "$_ls_gap" ]; then _lw_term=1; else [ -z "$_ls_pid" ] || kill -KILL "$_ls_pid" 2>/dev/null; exit 0; fi' TERM
    # 續約請求的逾時：隔多久續一次的一半，夾在 1～10 秒（正常的 TTL 下就是 10 秒）。負載七八十的機器上 daemon 回個續約
    # 常常超過五秒，逾時太短就把「慢」當成「死」。
    _lw_m=$((_renew_every / 2))
    [ "$_lw_m" -ge 1 ] || _lw_m=1
    [ "$_lw_m" -le 10 ] || _lw_m=10
    # 續約失敗之後改成短間隔重試（不是再等一整個續約間隔）：TTL 180 秒時整個間隔是 60 秒，以前失敗之後只剩兩次機會
    # （60、120 秒），daemon 慢兩分鐘就把完全合法的建置整棵殺掉；現在用到「再試一次也來得及」的最後一刻才放棄。
    _lw_retry=5
    [ "$_lw_retry" -le "$_renew_every" ] || _lw_retry=$_renew_every
    _lw_gap=""
    while :; do
        [ -d "$_state" ] || return 0
        am_lease_sleep "${_lw_gap:-$_renew_every}"
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
            if [ -n "$_lw_sent" ]; then
                _deadline=$((_lw_sent + _ttl))
            else
                # The renewal succeeded, but the pre-request clock read failed. A post-response time still gives a safe lower
                # bound: the daemon renewed no earlier than this request's start, at most _lw_m seconds before the response.
                _lw_done=$(am_epoch)
                if [ -n "$_lw_done" ]; then _deadline=$((_lw_done - _lw_m + _ttl)); else _deadline=""; fi
            fi
            _lw_gap=""
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
        # 下一次重試（隔 `_lw_retry`、最久再加 curl 逾時）會落在保守的 deadline 之後才停；還來得及就繼續試。
        if [ -z "$_lw_now" ] || [ -z "$_deadline" ] || [ $((_deadline - _lw_now)) -le $((_lw_retry + _lw_m)) ]; then
            am_lease_lost "續約一直失敗，名額快到期了"
            return 0
        fi
        _lw_gap=$_lw_retry
    done
}

# ── 租約狀態目錄的回收（issue #154）───────────────────────────────────────────────────────
#
# 每次租約在 `${TMPDIR:-/tmp}` 底下建一個 `am-cargo-lease.<shim 的 pid>.<隨機>` 目錄放 cargo 的 pid 與失效標記。正常結束、
# 租約失效、shim 被單獨 SIGKILL（續約守衛會接手收尾，見 am_lease_watch）都會清；但**整個 pane 被關**（process
# group 一起被 SIGKILL）時 trap 與守衛都沒機會跑，目錄就一直留著。所以每次 shim 啟動時順手收掉「沒人在用」的。

# 這個租約狀態目錄還有人在用嗎（`$1` 是目錄）。建它的 shim（名字裡的 pid）或前景的 cargo（`pid` 檔）任何一個還活著就是在用——
# shim 自己被 SIGKILL 時守衛與 cargo 還在跑，cargo 的目錄不能在它腳下刪掉。只剩續約守衛活著時清掉無妨：它發現 shim 與 cargo 都不在了，
# 就只是放名額、收目錄（目錄已經沒了照樣收得完）。pid 被別的行程借用（回收重用）只會讓目錄多留一陣子，不會誤刪；判斷不出來一律當成在用。
#
# 舊版 shim 的目錄名字裡沒有 pid（`am-cargo-lease.<隨機>`）：只能看裡面記的 pid，再加上「放夠久」——年輕的可能正在被建起來。
am_lease_dir_in_use() {
    _u_rest=${1##*/}
    _u_rest=${_u_rest#am-cargo-lease.}
    _u_owner=""
    case "$_u_rest" in
        [0-9]*.*) _u_owner=${_u_rest%%.*} ;;
    esac
    case "$_u_owner" in *[!0-9]*) _u_owner="" ;; esac
    for _u_pid in "$_u_owner" "$(cat "$1/pid" 2>/dev/null)"; do
        case "$_u_pid" in '' | *[!0-9]*) continue ;; esac
        kill -0 "$_u_pid" 2>/dev/null && return 0
    done
    if [ -z "$_u_owner" ]; then
        # 舊格式：一小時內動過的就不碰。`find` 出錯（不支援 -mmin）也當成在用。
        _u_young=$(find "$1" -maxdepth 0 -mmin -60 2>/dev/null) || return 0
        [ -z "$_u_young" ] || return 0
    fi
    return 1
}

# 收掉 `${TMPDIR:-/tmp}` 底下沒人在用的 `am-cargo-lease.*`。只動自己名下的**目錄**（不碰符號連結、檔案、別人的）。
am_sweep_stale_leases() {
    _sw_base="${TMPDIR:-/tmp}"
    _sw_base="${_sw_base%/}"
    for _sw_d in "$_sw_base"/am-cargo-lease.*; do
        [ -d "$_sw_d" ] && [ ! -L "$_sw_d" ] && [ -O "$_sw_d" ] || continue
        am_lease_dir_in_use "$_sw_d" && continue
        rm -rf "$_sw_d" 2>/dev/null
    done
    return 0
}

# 前景跑指令的包裝：先把自己的 pid 寫給守衛，再 `exec` 成那個指令（pid 不變，所以記下來的就是 cargo 本人）。
# 前景執行不能換成背景：非互動 shell 的背景指令會忽略 SIGINT／SIGQUIT（Ctrl-C 就停不下 cargo，
# `cargo run` 起來的程式也繼承到「忽略 SIGINT」），stdin 也會被換成 /dev/null。
_guard_sh='printf "%s" "$$" > "$1" 2>/dev/null; shift; exec "$@"'

# 叫 shim 的那一端（呼叫端）還在不在。`$PPID` 是 shell 啟動那一刻的父行程：呼叫端在 shim 起來之前就走了的話，
# 那已經是**收養者**——launchd／init（pid 1，非 root 對它 `kill -0` 回 EPERM，碰巧判成「不在」），或 Linux 的
# `systemd --user`（child subreaper、同一個使用者，`kill -0` 會成功）。以前在 systemd 底下 shim 就這樣把收養者
# 當成呼叫端、永遠等名額（issue #676）。收養者一律當成沒有呼叫端；認的是名字，因為 subreaper 旗標從外面讀不到。
am_caller_gone() {
    [ "${_parent:-0}" -gt 1 ] 2>/dev/null || return 0
    case "${_parent_comm##*/}" in systemd | launchd | init) return 0 ;; esac
    ! kill -0 "$_parent" 2>/dev/null
}

am_cargo() {
    # 遞迴保險絲：萬一 `am_is_shim` 認不出某一份 shim（被改過檔頭、或別的專案裝了同名 wrapper），
    # 兩份 shim 會互相把對方當成真 cargo 一路 fork 下去。寧可大聲失敗，也不要 fork 到機器躺平。
    AM_SHIM_DEPTH=$((${AM_SHIM_DEPTH:-0} + 1))
    export AM_SHIM_DEPTH
    _parent=$PPID
    _parent_comm=$(ps -o comm= -p "$_parent" 2>/dev/null)
    if [ "$AM_SHIM_DEPTH" -gt 4 ]; then
        printf 'agents-manager: cargo shim 遞迴 %s 層——PATH 上有多份 shim 而且認不出來。把真 cargo 放進 AM_REAL_CARGO 再跑一次。\n' "$AM_SHIM_DEPTH" >&2
        exit 127
    fi
    _real=$(am_real_cargo | head -n 1)
    if [ -z "$_real" ]; then
        printf 'agents-manager: 找不到真正的 cargo（把它的路徑放進 AM_REAL_CARGO）\n' >&2
        exit 127
    fi
    _sub=$(am_cargo_subcommand "$@")
    if ! am_cargo_is_heavy "$_sub"; then
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
        if am_is_managed; then
            am_scheduler_unusable "Bot pane 缺少 AM_BOT_ID 或 AM_BOT_TOKEN，拒絕改用共用 User token"
        fi
        if [ -n "${AM_BOT_ID:-}" ] || [ -n "${AM_BOT_TOKEN:-}" ] || [ -n "${AM_HOOK_TOKEN:-}" ]; then
            printf 'agents-manager: 遠端 Bot 沒有 AM_PORT 或 bot credential，不問本機 scheduler，直接跑 cargo\n' >&2
            exec "$_real" "$@"
        fi
        printf 'agents-manager: 沒有 bot／管理員身分，這次 cargo 不經過排程器，直接跑\n' >&2
        exec "$_real" "$@"
    }
    # 不知道 daemon 在哪（遠端 bot 的 pane）：不猜位址、一通 curl 都不打，明講原因、直接跑。遠端那台的編譯本來就
    # 不受這台 daemon 的名額管；外部編譯（`remote-cargo`）也用不上——那是本機 pane 的事，遠端 pane 沒有 helper 的環境變數。
    _port=$(am_build_port) || {
        printf 'agents-manager: 這個 pane 有 bot 身分卻沒有 AM_PORT（遠端主機連不到 daemon，或環境變數沒帶進來），不知道 daemon 在哪、不會去猜 127.0.0.1，這次 cargo 不排程，直接跑\n' >&2
        exec "$_real" "$@"
    }
    # 順手收掉沒人在用的租約狀態目錄（issue #154）：被 SIGKILL 的 shim 沒機會自己清。
    am_sweep_stale_leases
    _holder="${AM_AGENT_NAME:-manual}:$$"
    _bot_id="${AM_BOT_ID:-}"
    _purpose=$(printf '%s' "$*" | cut -c1-200)
    _url="http://127.0.0.1:${_port}/build-slots"

    # 外部 Cargo worker（issue #104）：會轉到遠端的指令**不佔本機的建置名額**（issue #155）——名額管的是本機的
    # RAM／CPU，遠端編譯不吃它；以前先拿名額再決定轉不轉，`max_concurrent=2` 時全部走遠端也只能同時跑 2 個。
    # helper 本身讀 config + 0600 secret file，pane 不會拿到 SSH 密碼。
    # 125 = 設定在 pane 啟動後被關掉／這個指令不適合 offload（**還沒有**在遠端動手），退回本機：
    # 這時才落到下面去拿本機名額；其他非 0 = 遠端驗證真的失敗，原樣回報，不能偷偷改成本機成功。
    #
    # pane 缺 helper／config 的位置時**不能靜默退回本機**（issue #138）：整批子 agent 因此在本機排隊，
    # 而沒有人知道外部編譯根本沒生效。缺哪個就講哪個。
    if am_remote_cargo_eligible "$_sub"; then
        _remote_missing=$(am_remote_cargo_missing)
        if [ -n "$_remote_missing" ]; then
            printf 'agents-manager: 外部編譯沒有啟用：這個 pane 缺 %s（沒設，或指到的檔案不能執行），這次 %s 在本機跑\n' "$_remote_missing" "${_sub:-cargo}" >&2
        else
            if [ -n "${AM_DATA_DIR:-}" ]; then
                "$AM_DAEMON_EXE" remote-cargo --config "$AM_CONFIG_PATH" --data-dir "$AM_DATA_DIR" --cwd "$PWD" -- "$@"
            else
                "$AM_DAEMON_EXE" remote-cargo --config "$AM_CONFIG_PATH" --cwd "$PWD" -- "$@"
            fi
            _remote_rc=$?
            [ "$_remote_rc" -eq 125 ] || exit "$_remote_rc"
        fi
    fi

    # 明講的 bypass（issue #128）：使用者／agent 自己決定這次不經過排程器（排程器壞了又急著編）。是明確的選擇，不是連線錯誤自動取得的。
    if [ -n "${AM_CARGO_BYPASS_SCHEDULER:-}" ]; then
        printf 'agents-manager: AM_CARGO_BYPASS_SCHEDULER 有設，這次 cargo 明確不經過排程器，直接跑\n' >&2
        exec "$_real" "$@"
    fi
    if ! command -v curl >/dev/null 2>&1; then
        am_scheduler_unusable "這台機器沒有 curl，問不了 build scheduler"
        exec "$_real" "$@"
    fi

    # 問不到排程器（連不上、回應看不懂、身分被拒）時：受管的 bot 每 3 秒重試、最多等 `AM_BUILD_SCHEDULER_WAIT_SECS`（預設 120 秒，
    # daemon 重啟／升級夠了），之後 fail closed（exit 75）——**不自動變成沒有名額的 cargo**（issue #128）；排程器回來了就照常排隊。
    # 人工 shell（沒有 bot 身分）維持明講的 bypass：直接跑。名額滿了（granted:false）是排程器有在回答，另一條路：一直等到有名額。
    _wait=${AM_BUILD_SCHEDULER_WAIT_SECS:-120}
    case "$_wait" in *[!0-9]* | '') _wait=120 ;; esac
    _down_max=$((_wait / 3))
    [ "$_down_max" -ge 1 ] || _down_max=1
    _attempt=0
    _down=0
    while :; do
        _acq_at=$(am_epoch)
        if [ -n "${AM_BOT_ID:-}" ]; then
            _resp=$(curl -s -m 5 -X POST "${_url}/acquire" \
                -H "$_auth" -H "X-AM-Bot-Id: ${AM_BOT_ID}" \
                --data-urlencode "holder=${_holder}" \
                --data-urlencode "bot_id=${_bot_id}" \
                --data-urlencode "purpose=${_purpose}" 2>/dev/null)
        else
            _resp=$(curl -s -m 5 -X POST "${_url}/acquire" \
                -H "$_auth" \
                --data-urlencode "holder=${_holder}" \
                --data-urlencode "bot_id=${_bot_id}" \
                --data-urlencode "purpose=${_purpose}" 2>/dev/null)
        fi
        _rc=$?
        _bad=""
        _fatal=""
        if [ "$_rc" -ne 0 ] || [ -z "$_resp" ]; then
            _bad="連不上（daemon 沒開？curl 結束碼 ${_rc}）"
        else
            case "$_resp" in
                *'"granted":true'*) break ;;
                *'"granted":false'*)
                    _attempt=$((_attempt + 1))
                    _down=0
                    if [ "$_attempt" = 1 ]; then
                        printf 'agents-manager: 全機的 cargo 名額滿了，等一個空出來（waiting_for_build_slot）……\n' >&2
                    fi
                    # 呼叫端（叫我們的 shell／agent）已經不在了：沒有人在等這次建置，不要留一個永遠在等名額的孤兒。
                    if am_caller_gone; then
                        printf 'agents-manager: 呼叫端已經結束，不再等 cargo 名額\n' >&2
                        exit 1
                    fi
                    _retry=$(am_json_field retry_after_secs "$_resp")
                    case "$_retry" in *[!0-9]* | '') _retry=5 ;; esac
                    sleep "$_retry"
                    continue
                    ;;
                # 身分被拒不會在幾秒內自己好：不必等滿再說。
                *'"error":"unauthorized"'*)
                    _bad="不認得這顆 bot 的身分（unauthorized）"
                    _fatal=1
                    ;;
                # 這個 holder 是別顆 bot 的（issue #460 的守衛）：`<agent 名>:<pid>` 撞到還沒過期的舊列，
                # 等下去也只是等那一列的 TTL，而且訊息會變成沒人看得懂的「回應看不懂」。同 unauthorized，當場停。
                *'"reason":"holder_bot_mismatch"'*)
                    _bad="這個名額的 holder 是別顆 bot 的（holder_bot_mismatch）：<agent 名>:<pid> 撞到還沒過期的舊列，等它到期再跑"
                    _fatal=1
                    ;;
                *) _bad="回應看不懂：$(printf '%s' "$_resp" | cut -c1-200)" ;;
            esac
        fi
        if ! am_is_managed; then
            printf 'agents-manager: build scheduler %s，這次不排程，直接跑（沒有 bot 身分的人工 shell）\n' "$_bad" >&2
            exec "$_real" "$@"
        fi
        _down=$((_down + 1))
        if [ -n "$_fatal" ] || [ "$_down" -ge "$_down_max" ]; then
            printf 'agents-manager: build scheduler %s；受管的 bot 不會在沒有名額的情況下跑 cargo（會突破 max_concurrent，issue #128）。稍後重試，或明確繞過：AM_CARGO_BYPASS_SCHEDULER=1\n' "$_bad" >&2
            [ -z "$_fatal" ] || exit 77
            exit 75
        fi
        if [ "$_down" = 1 ]; then
            printf 'agents-manager: build scheduler %s；受管的 bot 不會在沒有名額時跑 cargo，每 3 秒重試、最多等 %s 秒……\n' "$_bad" "$_wait" >&2
        fi
        if am_caller_gone; then
            printf 'agents-manager: 呼叫端已經結束，不再等 build scheduler\n' >&2
            exit 1
        fi
        sleep 3
    done

    _token=$(am_json_field token "$_resp")
    _jobs=$(am_json_field cargo_jobs "$_resp")
    _ttl=$(am_json_field lease_ttl_secs "$_resp")
    case "$_jobs" in *[!0-9]* | '') _jobs=2 ;; esac
    case "$_ttl" in *[!0-9]* | '') _ttl=180 ;; esac
    # 測試執行緒上限跟著名額走（issue #813）：呼叫端自己設了 `RUST_TEST_THREADS` 就尊重（`--test-threads` 本來就優先於環境變數）；
    # 沒設才注入名額回應的 `test_threads`。舊 daemon 的回應沒有這個欄位＝用預設 8；`0`＝daemon 明講不設，交給 libtest 的預設。
    _test_env=""
    if am_cargo_runs_tests "$_sub" && [ -z "${RUST_TEST_THREADS:-}" ]; then
        _tt=$(am_json_field test_threads "$_resp")
        case "$_tt" in *[!0-9]* | '') _tt=8 ;; esac
        [ "$_tt" -eq 0 ] || _test_env="RUST_TEST_THREADS=$_tt"
    fi
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
    _state=$(mktemp -d "${TMPDIR:-/tmp}/am-cargo-lease.$$.XXXXXX" 2>/dev/null)
    if [ -z "$_state" ] || [ ! -d "$_state" ]; then
        curl -s -m 5 -X POST "${_url}/release" --data-urlencode "holder=${_holder}" --data-urlencode "token=${_token}" >/dev/null 2>&1
        am_scheduler_unusable "建不出暫存目錄，沒辦法在名額失效時停掉 cargo"
        exec "$_real" "$@"
    fi
    # shim 自己的 process group：守衛在 shim 被 SIGKILL 之後靠它確認 cargo 還是自己的（`am_kill_tree`，issue #183）。
    _pgid=$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' ')
    # SIGSTOP 凍住行程樹（`am_kill_tree`）時，如果這一組行程剛好是「孤兒 process group」（沒有成員的父行程在同 session 的別組——
    # 沒有控制終端的 CI／launchd／`nohup`，或呼叫端是背景行程時），kernel 會對整組送 SIGHUP＋SIGCONT，連 shim 自己都收到：
    # shim 被 HUP 殺掉、退出碼是 -1／129，呼叫端拿不到可重試的 75（CI 上間歇紅）。用「執行 `:`」而不是「忽略」：
    # 有 handler 的訊號在 exec 時會還原成預設，所以 cargo／rustc 仍照常吃終端機掛斷，只有 shim 與守衛不被這個假的 HUP 帶走。
    trap ':' HUP
    am_lease_watch &
    _renew_pid=$!
    trap '_release' EXIT INT TERM

    # 租約已經失效（前景那個被我們停掉，或是在 cargo 起來之前就失效）：收尾、告訴使用者為什麼、退 75（可重試）。
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

    # 沒有名額就不起本機 cargo（租約在起 cargo 之前就失效了：守衛那時還沒有 pid 可以停）。
    if [ -s "$_state/lost" ]; then
        am_lease_lost_exit
    fi

    # `$_test_env` 不加引號：空的就不展開成任何參數，有值時是一個不含空白的 `RUST_TEST_THREADS=N`。
    env AM_BUILD_SLOT_HELD=1 AM_REAL_CARGO="$_real" CARGO_BUILD_JOBS="$_jobs" $_test_env \
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
