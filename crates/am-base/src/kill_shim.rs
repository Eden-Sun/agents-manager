//! `pkill`／`killall` 的 PATH shim（issue #1884 第二階段）：bot 的 pane 打 `pkill …`／`killall …` 時，先用跟
//! [`crate::kill_guard`] 同一份規則判斷會不會殺一大片行程，確定廣域才擋，其餘一律 exec 真的。
//!
//! 為什麼要有 shim：PreToolUse hook 只蓋得到有接線的 CLI 的本機 bot（子 claude、遠端、codex、grok 都沒有），
//! 而 PATH 上的 shim 子 pane 繼承、遠端也有；argv 已經被 shell 切好，不必自己解析指令字串。
//!
//! 內容是**一支 POSIX sh**（macOS 的 `/bin/sh` 與 Linux 的 dash 都跑得動；不呼叫 daemon、不連網路、只用 `grep`／`tr`），
//! `pkill`、`killall` 兩個檔名同一份內容，以 `basename $0` 分派。保護名單、門檻、訊號名單**從 `kill_guard` 的常數代入**
//! （[`shim_sh`]），不另抄一份；判斷邏輯是 `kill_guard::check_pkill`／`check_killall` 的 sh 版，
//! 兩邊靠 parity 測試（同一張案例表）保持一致。
//!
//! 判斷與 `kill_guard` 一樣**不分作業系統**：選項寫在 pattern 之後，BSD（macOS）當 pattern、procps（Linux）當選項，兩邊語意不同，
//! 寫法本身就不可攜，所以兩邊都擋；shim 因此不必依 `uname` 分岔，也不會跟 PreToolUse hook 對同一條指令給出不同答案。
//!
//! 限制（SPEC §4.4c）：`kill` 是 shell 內建，shim 擋不到 `kill -9 -1`／`kill 0`（留給 PreToolUse）；
//! `/usr/bin/pkill` 這種絕對路徑會繞過 shim；擋不擋以靜態字串判斷，判不出來（regex 語法錯、pattern 超過長度上限）一律放行。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// shim 的檔名（`pkill`、`killall` 同一份內容）。
pub const NAMES: [&str; 2] = ["pkill", "killall"];

/// 退出碼：被擋。不是 0／1——1 是 pkill 的「沒有行程符合」，腳本會把它當成正常。
pub const DENIED_EXIT: i32 = 2;

const TEMPLATE: &str = r####"#!/bin/sh
# agents-manager pkill/killall guard shim (issue #1884). Installed at the front of a managed pane's PATH,
# next to the herdr and cargo shims; the same file is installed as `pkill` and as `killall`.
#
# POSIX sh only, no `set -e`: this guard must never be the reason a harmless command fails, so anything it cannot
# judge (unparsable pattern, over-long pattern, no real tool found is the one exception) goes straight to the real tool.
# The rules are the same as the Rust `kill_guard` (SPEC §4.4c); the constants right below are filled in from it at install time.
#
# AM_SHIM_MARKER: 這行讓 shim 認得出「PATH 上那個 pkill 其實是我自己」（am_is_shim）。不要刪。

AM_PROTECTED_NAMES='@AM_PROTECTED_NAMES@'
AM_PROTECTED_CMDLINES='@AM_PROTECTED_CMDLINES@'
AM_MIN_PATTERN=@AM_MIN_PATTERN@
AM_MAX_PATTERN=@AM_MAX_PATTERN@
AM_MAX_GROUP=@AM_MAX_GROUP@
AM_MAX_REGEX_PATTERNS=@AM_MAX_REGEX_PATTERNS@
AM_SIGNAMES='@AM_SIGNAMES@'
AM_HOW_TO='@AM_HOW_TO@'
AM_NODE_NOTE='@AM_NODE_NOTE@'
AM_NL='
'
# 字元數、字元範圍一律用位元組語意（跟 Rust 那邊「可判斷語法只有可印 ASCII」對齊）；exec 真的之前還原。
AM_LC_SET=${LC_ALL+x}
AM_LC_OLD=${LC_ALL-}
LC_ALL=C

am_is_shim() {
    head -n 12 "$1" 2>/dev/null | grep -q 'AM_SHIM_MARKER' 2>/dev/null
}

am_is_bot_bin_dir() {
    case "${1%/}" in
        */bots/*/bin) return 0 ;;
        *) return 1 ;;
    esac
}

# 真的 pkill／killall：PATH 上第一個不是 shim（檔頭有 marker）、也不在任何一顆 bot 的 `bots/<id>/bin` 的；
# 找不到再試標準位置。同一條 PATH 上常掛著好幾顆 bot 的 shim 目錄，不能只跳過自己那個（見 cargo shim 的同一段）。
am_real() {
    _self=$(cd "$(dirname "$0")" 2>/dev/null && pwd)
    {
        printf '%s\n' "$PATH" | tr ':' '\n'
        printf '%s\n' /usr/bin /bin /usr/sbin /sbin
    } | {
        while IFS= read -r _d; do
            [ -n "$_d" ] || _d=.
            _abs=$(cd "$_d" 2>/dev/null && pwd) || continue
            [ "$_abs" = "$_self" ] && continue
            am_is_bot_bin_dir "$_abs" && continue
            if [ -x "$_abs/$1" ] && ! am_is_shim "$_abs/$1"; then
                printf '%s\n' "$_abs/$1"
                break
            fi
        done
    }
}

am_deny() {
    printf 'agents-manager: 擋下 %s：%s\n%s\n' "$AM_TOOL" "$1" "$AM_HOW_TO" >&2
    exit @AM_DENIED_EXIT@
}

# ── 解析 ──────────────────────────────────────────────────────────────
# am_addp <字> <是否在 `--` 之後>：記下一個 pattern（P_<i>／PD_<i>，字可以是空字串）。
am_addp() {
    P_N=$((P_N + 1))
    eval "P_$P_N=\$1"
    eval "PD_$P_N=\$2"
}

# am_setv <鍵> <值>：記下帶值的選項（鍵 `s:<字元>`／`l:<長名>`）。
am_setv() {
    VALS="$VALS$1=$2$AM_NL"
}

# am_val <鍵>…：有沒有這些選項裡任何一個的值；有就設 VAL 並回 0。
am_val() {
    for _k in "$@"; do
        while IFS= read -r _line; do
            case $_line in
                "$_k="*)
                    VAL=${_line#*=}
                    return 0
                    ;;
            esac
        done <<AM_VALS_EOF
$VALS
AM_VALS_EOF
    done
    return 1
}

am_flag() {
    for _k in "$@"; do
        case $FLAGS in *" $_k "*) return 0 ;; esac
    done
    return 1
}

# am_parse <bsd|gnu> <帶值短選項字元> <帶值長選項（空白分隔）> 參數…
# bsd＝第一個非選項參數起全是 pattern（BSD getopt）；gnu＝選項可以夾在 pattern 之間。跟 Rust 的 `parse_args` 同一套。
am_parse() {
    _mode=$1
    _vs=$2
    _vl=$3
    shift 3
    P_N=0
    FLAGS=' '
    VALS=''
    while [ "$#" -gt 0 ]; do
        _a=$1
        shift
        if [ "$_a" = "--" ]; then
            while [ "$#" -gt 0 ]; do
                am_addp "$1" 1
                shift
            done
            break
        fi
        case $_a in
            -?*) ;;
            *)
                am_addp "$_a" 0
                if [ "$_mode" = bsd ]; then
                    while [ "$#" -gt 0 ]; do
                        am_addp "$1" 0
                        shift
                    done
                fi
                continue
                ;;
        esac
        _s=${_a#-}
        # 訊號：`-9`、`-TERM`、`-SIGKILL`
        case $_s in
            *[!0-9]*) ;;
            *) continue ;;
        esac
        case " $AM_SIGNAMES " in *" ${_s#SIG} "*) continue ;; esac
        case $_a in
            --*)
                _l=${_a#--}
                case $_l in
                    *=*) am_setv "l:${_l%%=*}" "${_l#*=}" ;;
                    *)
                        case " $_vl " in
                            *" $_l "*)
                                if [ "$#" -gt 0 ]; then
                                    am_setv "l:$_l" "$1"
                                    shift
                                else
                                    FLAGS="${FLAGS}l:$_l "
                                fi
                                ;;
                            *) FLAGS="${FLAGS}l:$_l " ;;
                        esac
                        ;;
                esac
                continue
                ;;
        esac
        _cl=$_s
        while [ -n "$_cl" ]; do
            _c=${_cl%"${_cl#?}"}
            _cl=${_cl#?}
            case $_vs in
                *"$_c"*)
                    if [ -n "$_cl" ]; then
                        am_setv "s:$_c" "$_cl"
                    elif [ "$#" -gt 0 ]; then
                        am_setv "s:$_c" "$1"
                        shift
                    fi
                    break
                    ;;
            esac
            FLAGS="${FLAGS}s:$_c "
        done
    done
}

# ── pattern 判斷 ──────────────────────────────────────────────────────
# am_in_subset <pattern>：在不在「可判斷語法」裡（Rust `kill_guard::in_common_subset` 的 sh 版，規則逐條一樣，見該處說明）。
# 在裡面的語法，Rust 的迷你 ERE 與 `grep -E`（GNU／BSD）語意相同；不在裡面就是「判不出來」，呼叫端放行。
am_in_subset() {
    _s=$1
    [ -n "$_s" ] && [ "${#_s}" -le "$AM_MAX_PATTERN" ] || return 1
    case $_s in *[![:print:]]*) return 1 ;; esac
    _depth=0
    _prev=S
    while [ -n "$_s" ]; do
        _c=${_s%"${_s#?}"}
        _s=${_s#?}
        if [ "$_prev" = E ]; then
            case $_c in '|' | ')') ;; *) return 1 ;; esac
        fi
        case $_c in
            '\')
                _n=${_s%"${_s#?}"}
                case $_n in
                    '.' | '*' | '+' | '?' | '|' | '(' | ')' | '[' | ']' | '^' | '$' | '{' | '}' | '\') ;;
                    *) return 1 ;;
                esac
                _s=${_s#?}
                _prev=A
                ;;
            '[')
                case $_s in '^'*) _s=${_s#?} ;; esac
                _tok=0
                while :; do
                    [ -n "$_s" ] || return 1
                    case $_s in
                        ']'*) break ;;
                        0-9* | a-z* | A-Z*) _s=${_s#???} ;;
                        ' '*) _s=${_s#?} ;;
                        [A-Za-z0-9_./:,@=]*) _s=${_s#?} ;;
                        *) return 1 ;;
                    esac
                    _tok=$((_tok + 1))
                done
                [ "$_tok" -gt 0 ] || return 1
                _s=${_s#?}
                _prev=A
                ;;
            '(')
                _depth=$((_depth + 1))
                [ "$_depth" -le "$AM_MAX_GROUP" ] || return 1
                _prev=S
                ;;
            ')')
                [ "$_depth" -gt 0 ] || return 1
                case $_prev in A | Q) ;; *) return 1 ;; esac
                _depth=$((_depth - 1))
                _prev=A
                ;;
            '|')
                case $_prev in A | Q | E) ;; *) return 1 ;; esac
                _prev=S
                ;;
            '*' | '+' | '?')
                [ "$_prev" = A ] || return 1
                _prev=Q
                ;;
            '^')
                [ "$_depth" -eq 0 ] && [ "$_prev" = S ] || return 1
                _prev=B
                ;;
            '$')
                [ "$_depth" -eq 0 ] || return 1
                case $_prev in A | Q | B) ;; *) return 1 ;; esac
                _prev=E
                ;;
            '{' | '}' | ']') return 1 ;;
            *) _prev=A ;;
        esac
    done
    [ "$_depth" -eq 0 ] || return 1
    case $_prev in A | Q | E) return 0 ;; *) return 1 ;; esac
}

# am_broad <pattern> <full> <exact> <icase> <可做regex比對>：pattern 太短、或會命中受保護的行程就回 0（原因放 BROAD_WHY）。
# regex 只在可判斷語法裡比對（`grep -E`，線性時間、不會災難性回溯）；語法在範圍外、或超過一條指令最多比對的 pattern 數 → 判不出來，回 1（放行）。
am_broad() {
    _pat=$1
    _full=$2
    _exact=$3
    _ic=$4
    _rok=$5
    if [ "$_exact" != 1 ] && [ "${#_pat}" -lt "$AM_MIN_PATTERN" ]; then
        BROAD_WHY="只有 ${#_pat} 個字元，會命中大量不相干的行程（例如命令列含該字串的所有 agent）"
        return 0
    fi
    [ "$_rok" = 1 ] || return 1
    am_in_subset "$_pat" || return 1
    if [ "$_full" = 1 ]; then
        _subj=$AM_PROTECTED_CMDLINES
    else
        _subj=$AM_PROTECTED_NAMES
    fi
    _gopts=-E
    [ "$_ic" = 1 ] && _gopts=${_gopts}i
    [ "$_exact" = 1 ] && _gopts=${_gopts}x
    _out=$(printf '%s\n' "$_subj" | grep "$_gopts" -e "$_pat" 2>/dev/null)
    [ $? -eq 0 ] || return 1
    _hit=${_out%%"$AM_NL"*}
    [ -n "$_hit" ] || return 1
    if [ "$_full" = 1 ]; then
        _w=${_hit%% *}
        _hit=${_w##*/}
    fi
    BROAD_WHY="會命中受保護的 agent 行程（\`$_hit\`）"
    [ "$_hit" = node ] && BROAD_WHY="$BROAD_WHY$AM_NODE_NOTE"
    return 0
}

# ── pkill ─────────────────────────────────────────────────────────────
am_check_pkill() {
    _pk_short='PUugGstTFJjMN'
    _pk_long='parent euid uid pgroup group session terminal signal pidfile runstates ns nslist older newer'
    am_parse gnu "$_pk_short" "$_pk_long" "$@"
    _gn=$P_N
    am_parse bsd "$_pk_short" "$_pk_long" "$@"
    # BSD 的 getopt 在第一個非選項參數就停：它之後、`--` 之前以 `-` 開頭的字，BSD 當 pattern、procps 當選項。
    if [ "$P_N" -ne "$_gn" ]; then
        _ad_n=0
        _seen=0
        for _x in "$@"; do
            if [ "$_seen" = 1 ]; then
                _ad_n=$((_ad_n + 1))
                eval "AD_$_ad_n=\$_x"
            elif [ "$_x" = "--" ]; then
                _seen=1
            fi
        done
        _bad=
        _i=1
        while [ "$_i" -le "$P_N" ]; do
            eval "_p=\$P_$_i"
            case $_p in
                -?*)
                    _after=0
                    _j=1
                    while [ "$_j" -le "$_ad_n" ]; do
                        eval "_y=\$AD_$_j"
                        [ "$_y" = "$_p" ] && _after=1
                        _j=$((_j + 1))
                    done
                    [ "$_after" = 1 ] || _bad="$_bad $_p"
                    ;;
            esac
            _i=$((_i + 1))
        done
        if [ -n "$_bad" ]; then
            am_deny "選項${_bad} 寫在比對字串之後。macOS／BSD 的 pkill 遇到第一個非選項參數就停止解析選項，它們會被當成**額外的比對字串**（\`pkill -f x -P 1\` 會變成殺掉所有命令列含「1」的行程，2026-10-21 #1884 因此殺光整台機器的 agent）；Linux 的 pkill 卻當選項，兩邊行為不同。選項一律寫在 pattern 之前（\`pkill -P 1 -f x\`）。"
        fi
    fi
    _full=0
    _exact=0
    _ic=0
    am_flag s:f l:full && _full=1
    am_flag s:x l:exact && _exact=1
    am_flag s:i l:ignore-case && _ic=1
    if [ "$P_N" -eq 0 ]; then
        if am_val s:u s:U s:G s:g s:s s:t l:euid l:uid l:group l:pgroup l:session l:terminal; then
            am_deny "沒有比對字串，只用 -u／-g／-s／-t／-P 1 這類條件，會殺掉一整批行程（含其他 agent）。"
        fi
        if am_val s:P l:parent; then
            case $VAL in
                0 | 1) am_deny "沒有比對字串，只用 -u／-g／-s／-t／-P 1 這類條件，會殺掉一整批行程（含其他 agent）。" ;;
            esac
        fi
        return 0
    fi
    _i=1
    while [ "$_i" -le "$P_N" ]; do
        eval "_p=\$P_$_i"
        _rok=0
        [ "$_i" -le "$AM_MAX_REGEX_PATTERNS" ] && _rok=1
        if am_broad "$_p" "$_full" "$_exact" "$_ic" "$_rok"; then
            am_deny "比對字串 \`$_p\` $BROAD_WHY。"
        fi
        _i=$((_i + 1))
    done
    return 0
}

# ── killall ───────────────────────────────────────────────────────────
am_signalish() {
    _t=${1#SIG}
    [ -n "$_t" ] || return 1
    case $_t in
        *[!ABCDEFGHIJKLMNOPQRSTUVWXYZ0-9]*) return 1 ;;
    esac
    return 0
}

am_check_killall() {
    # `-s`：Linux 是帶訊號的選項、BSD 是「只顯示」旗標；後面接得像訊號才當帶值。
    _n=$#
    while [ "$_n" -gt 0 ]; do
        _a=$1
        shift
        _n=$((_n - 1))
        if [ "$_a" = "-s" ] && [ "$_n" -gt 0 ] && am_signalish "$1"; then
            shift
            _n=$((_n - 1))
            continue
        fi
        set -- "$@" "$_a"
    done
    am_parse gnu 'uoyZtc' 'user older-than younger-than context' "$@"
    _re=0
    _ic=0
    am_flag s:r s:m l:regexp && _re=1
    am_flag s:I l:ignore-case && _ic=1
    if [ "$P_N" -eq 0 ]; then
        if am_val s:u l:user s:t s:c; then
            am_deny "沒有行程名稱、只用 -u／-t／-c 條件，會殺掉那個使用者／終端的所有行程（含其他 agent）。"
        fi
        return 0
    fi
    _i=1
    while [ "$_i" -le "$P_N" ]; do
        eval "_p=\$P_$_i"
        if [ "$_re" = 1 ]; then
            _rok=0
            [ "$_i" -le "$AM_MAX_REGEX_PATTERNS" ] && _rok=1
            if am_broad "$_p" 0 0 "$_ic" "$_rok"; then
                am_deny "\`$_p\` $BROAD_WHY。"
            fi
        else
            _lp=$_p
            [ "$_ic" = 1 ] && _lp=$(printf '%s' "$_p" | tr '[:upper:]' '[:lower:]')
            for _nm in $AM_PROTECTED_NAMES; do
                if [ "$_nm" = "$_lp" ]; then
                    _note=
                    [ "$_nm" = node ] && _note=$AM_NODE_NOTE
                    am_deny "\`$_p\` 就是受保護的行程名 \`$_nm\`（killall 會殺掉同名的全部行程，包含其他 agent 與使用者的 session）${_note}。"
                fi
            done
        fi
        _i=$((_i + 1))
    done
    return 0
}

AM_TOOL=$(basename "$0")
case $AM_TOOL in
    pkill) am_check_pkill "$@" ;;
    killall) am_check_killall "$@" ;;
esac

_real=$(am_real "$AM_TOOL")
if [ -z "$_real" ]; then
    printf 'agents-manager: 找不到真的 %s（PATH 上只有這支 shim，/usr/bin、/bin 也沒有）。\n' "$AM_TOOL" >&2
    exit 127
fi
if [ -n "$AM_LC_SET" ]; then
    LC_ALL=$AM_LC_OLD
    export LC_ALL
else
    unset LC_ALL
fi
exec "$_real" "$@"
"####;

/// 代入 `kill_guard` 常數後的 shim 內容（`pkill`、`killall` 共用）。
pub fn shim_sh() -> &'static str {
    static SH: OnceLock<String> = OnceLock::new();
    SH.get_or_init(|| {
        use crate::kill_guard::*;
        let signals = SIGNAL_NAMES.join(" ");
        let fills: [(&str, String); 10] = [
            ("@AM_PROTECTED_NAMES@", PROTECTED_NAMES.join("\n")),
            ("@AM_PROTECTED_CMDLINES@", PROTECTED_CMDLINES.join("\n")),
            ("@AM_MIN_PATTERN@", MIN_PATTERN_CHARS.to_string()),
            ("@AM_MAX_PATTERN@", MAX_PATTERN_CHARS.to_string()),
            ("@AM_MAX_GROUP@", MAX_GROUP_DEPTH.to_string()),
            ("@AM_MAX_REGEX_PATTERNS@", MAX_REGEX_PATTERNS.to_string()),
            ("@AM_SIGNAMES@", signals),
            ("@AM_HOW_TO@", HOW_TO.to_string()),
            ("@AM_NODE_NOTE@", NODE_NOTE.to_string()),
            ("@AM_DENIED_EXIT@", DENIED_EXIT.to_string()),
        ];
        let mut s = TEMPLATE.to_string();
        for (k, v) in fills {
            debug_assert!(!v.contains('\''), "代入單引號字串的值不能含單引號：{k}");
            s = s.replace(k, &v);
        }
        s
    })
}

/// 本機：`<bot_dir>/bin/pkill`、`bin/killall`（原子寫入、內容一樣不重寫，跟其他 shim 一致）。
pub fn install_local(bot_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = crate::shim_refresh::bin_dir(bot_dir);
    std::fs::create_dir_all(&dir)?;
    for name in NAMES {
        crate::shim_refresh::write_atomic(&dir.join(name), shim_sh())?;
    }
    Ok(dir)
}

/// 遠端：同一支原子同步腳本（issue #124）。
pub async fn install_remote(conn: &crate::hosts::HostConn, remote_bot_dir: &str) -> anyhow::Result<String> {
    let r = crate::shim_refresh::sync_remote(conn, &[remote_bot_dir.to_string()], &NAMES, true).await?;
    if !r.failed.is_empty() {
        anyhow::bail!("remote kill shim install failed: {:?}", r.failed);
    }
    Ok(format!("{remote_bot_dir}/bin"))
}
