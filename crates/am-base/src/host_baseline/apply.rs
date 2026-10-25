//! 一致性差異的補齊與還原（issue #719 階段 3a，SPEC §16.7）：**只做 `merge_key`、只補不蓋**。
//!
//! daemon 只產生腳本、解析結果；真正讀寫的是主機上的 `/bin/sh`（同 [`super::baseline_sh`]，遠端主機沒有 repo）。
//! - 只碰計畫裡的 `merge_key`：codex `config.toml`、claude 身份的 `settings.json`（要有 `jq`，沒有就降級 `manual`）。
//!   `write_file`（缺檔）與工具安裝維持 `manual`；codex 的 model／effort／service_tier 與 `.gitconfig` 憑證在這裡再擋一次。
//! - 鍵已存在（不論值）不改；檔不在不建；目標或它的目錄是 symlink 就拒絕。
//! - 寫之前把整個檔備份到主機上的 `~/.config/agents-manager/baseline-backup/<UTC>-<UUID>/`（目錄 0700、檔 0600），
//!   `manifest` 記每個檔原本在不在與權限；保留最新 [`KEEP_BACKUPS`] 份。備份**不拉回 daemon**，腳本只印標記行，
//!   不印路徑、不印內容。
//! - 新內容寫進同目錄 0600 暫存檔（`mktemp`），`sync` 後換回原本的權限再 `mv -f`。
//! - 還原時主機上的 `manifest` 是不可信輸入：先整份驗過（只認固定幾個目標、編號、權限），任一行不對整個拒絕，
//!   驗完才開始寫。

use super::plan::{Plan, MERGE_KEY};
use crate::hosts::sh_quote;
use serde::Serialize;

/// 備份目錄（相對主機的 `$HOME`）。探測腳本不讀這裡。
pub const BACKUP_ROOT_REL: &str = ".config/agents-manager/baseline-backup";
/// 主機上保留幾份備份。
pub const KEEP_BACKUPS: usize = 10;

/// 結果：寫了。
pub const APPLIED: &str = "applied";
/// 結果：鍵已經在了（不論值），沒動。
pub const PRESENT: &str = "present";
/// 結果：沒寫（`reason` 說為什麼）。
pub const SKIPPED: &str = "skipped";
/// 結果：這一項得手動補（例如主機沒有 `jq`）。
pub const MANUAL: &str = "manual";

/// 新的備份 id：`YYYYMMDDTHHMMSSZ-<UUID v4>`（小寫）。
pub fn new_backup_id() -> String {
    use rand::Rng;
    let mut b: [u8; 16] = rand::thread_rng().gen();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// 備份 id 的固定格式（[`new_backup_id`]）；不符就不能拿來拼主機上的路徑。
pub fn valid_backup_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.len() != 53 {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !(digits(0..8) && b[8] == b'T' && digits(9..15) && b[15] == b'Z' && b[16] == b'-') {
        return false;
    }
    b[17..].iter().enumerate().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => *c == b'-',
        _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
    })
}

fn backup_created_at(id: &str) -> Option<String> {
    if !valid_backup_id(id) {
        return None;
    }
    let created = chrono::NaiveDateTime::parse_from_str(&id[..16], "%Y%m%dT%H%M%SZ").ok()?;
    Some(created.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// 一項的結果（API 回應的 `results[]`）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Outcome {
    pub id: String,
    /// `applied` | `present` | `skipped` | `manual`
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Format {
    /// `table` 空字串＝根層的鍵。
    Toml { table: String, key: String, literal: String },
    Json { path: String, literal: String },
}

/// 要寫的一個鍵。`token` 是主機上的目標（`codex/config.toml`、`claude-default/settings.json`、`claude-cc<N>/settings.json`），
/// 由腳本自己換成路徑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyWrite {
    pub id: String,
    pub token: String,
    format: Format,
}

/// [`prepare`] 的結果：要送去主機寫的，和在 daemon 這邊就判定得手動的。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Prepared {
    pub writes: Vec<KeyWrite>,
    pub manual: Vec<Outcome>,
}

fn safe_seg(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// TOML 的值：只收 bool／整數／字串。字串用 JSON 的跳脫（也是合法的 TOML basic string）。
fn toml_literal(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Bool(_) => Some(v.to_string()),
        serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => Some(v.to_string()),
        serde_json::Value::String(_) => serde_json::to_string(v).ok(),
        _ => None,
    }
}

/// 從計畫挑出要寫的鍵。`ids` 省略＝計畫裡全部 `merge_key`；給了就每個都必須是計畫裡的 `merge_key`，否則回那個 id（400）。
pub fn prepare(plan: &Plan, ids: Option<&[String]>) -> Result<Prepared, String> {
    let fixable: Vec<_> = plan.actions.iter().filter(|a| a.action == MERGE_KEY).collect();
    let chosen: Vec<_> = match ids {
        None => fixable,
        Some(ids) => {
            let mut out = Vec::new();
            for id in ids {
                let Some(a) = fixable.iter().find(|a| &a.id == id) else { return Err(id.clone()) };
                if !out.iter().any(|o: &&super::plan::FixAction| o.id == a.id) {
                    out.push(*a);
                }
            }
            out
        }
    };
    let mut p = Prepared::default();
    for a in chosen {
        let manual = |reason| Outcome { id: a.id.clone(), result: MANUAL, reason: Some(reason) };
        // 憑證永遠手動（計畫本來就列 manual，這裡再擋一次）。
        if a.id.starts_with("gitconfig") || a.target.starts_with("git/") {
            p.manual.push(manual(super::plan::SECRET));
            continue;
        }
        let (Some(key), Some(value)) = (a.key.as_deref(), a.value.as_ref()) else {
            p.manual.push(manual(super::plan::NO_VALUE));
            continue;
        };
        if a.target == "codex/config.toml" {
            // codex 的 model／effort／service_tier：計畫裡不該有；有也整個略過，連 manual 都不列。
            if key.split('.').any(super::manifest::codex_forbidden) {
                continue;
            }
            let segs: Vec<&str> = key.split('.').collect();
            if segs.len() > 2 || !segs.iter().all(|s| safe_seg(s)) {
                p.manual.push(manual("unsupported_key"));
                continue;
            }
            let Some(literal) = toml_literal(value) else {
                p.manual.push(manual("unsupported_value"));
                continue;
            };
            let (table, key) = if segs.len() == 2 { (segs[0].to_string(), segs[1].to_string()) } else { (String::new(), segs[0].to_string()) };
            p.writes.push(KeyWrite { id: a.id.clone(), token: "codex/config.toml".into(), format: Format::Toml { table, key, literal } });
            continue;
        }
        if let Some(ident) = a.target.strip_prefix("claude:").and_then(|r| r.strip_suffix("/settings.json")) {
            // 只認 `~/.claude`（default）與 `~/.claude-cc<N>`：自訂的 `$CLAUDE_CONFIG_DIR`／alias 指到哪裡，主機上不一定找得回同一個目錄。
            let digits = ident.strip_prefix("cc").filter(|n| !n.is_empty() && n.len() <= 4 && n.bytes().all(|c| c.is_ascii_digit()));
            let token = match (ident, digits) {
                ("default", _) => "claude-default/settings.json".to_string(),
                (_, Some(n)) => format!("claude-cc{n}/settings.json"),
                _ => {
                    p.manual.push(manual("unsupported_identity"));
                    continue;
                }
            };
            if !key.split('.').all(safe_seg) {
                p.manual.push(manual("unsupported_key"));
                continue;
            }
            let literal = serde_json::to_string(value).unwrap_or_default();
            p.writes.push(KeyWrite { id: a.id.clone(), token, format: Format::Json { path: key.to_string(), literal } });
            continue;
        }
        p.manual.push(manual("unsupported_target"));
    }
    Ok(p)
}

/// apply 與 restore 共用：目標名單與路徑、權限。
const COMMON_SH: &str = r#"umask 077
ba_root="$HOME/.config/agents-manager/baseline-backup"
# 只認這幾個目標（宣告檔的 codex config.toml、claude settings.json）；不收絕對路徑、`..`、空白。
ba_token_ok() {
  case "$1" in ''|/*|*..*|*//*|*' '*) return 1 ;; esac
  case "$1" in
    codex/config.toml|claude-default/settings.json) return 0 ;;
    claude-cc*/settings.json)
      ba_t=${1#claude-cc}; ba_t=${ba_t%/settings.json}
      case "$ba_t" in ''|*[!0-9]*) return 1 ;; esac
      return 0 ;;
  esac
  return 1
}
ba_path() {
  ba_token_ok "$1" || return 1
  case "$1" in
    codex/config.toml) printf '%s/config.toml' "${CODEX_HOME:-$HOME/.codex}" ;;
    claude-default/settings.json) printf '%s/.claude/settings.json' "$HOME" ;;
    *) ba_t=${1%/settings.json}; printf '%s/.%s/settings.json' "$HOME" "$ba_t" ;;
  esac
}
# 權限（八進位三位）；讀不懂＝`-`（不 chmod）。
ba_mode() {
  ba_p=$(ls -ld "$1" 2>/dev/null | cut -c2-10)
  case "$ba_p" in [r-][w-][xsS-][r-][w-][xsS-][r-][w-][xtT-]) ;; *) printf -; return ;; esac
  ba_o=""
  for ba_i in 1 4 7; do
    ba_d=0
    [ "$(printf %s "$ba_p" | cut -c$ba_i)" = r ] && ba_d=$((ba_d + 4))
    [ "$(printf %s "$ba_p" | cut -c$((ba_i + 1)))" = w ] && ba_d=$((ba_d + 2))
    case "$(printf %s "$ba_p" | cut -c$((ba_i + 2)))" in x|s|t) ba_d=$((ba_d + 1)) ;; esac
    ba_o="$ba_o$ba_d"
  done
  printf %s "$ba_o"
}
ba_sync() { sync "$1" 2>/dev/null || sync; }
# 備份根目錄與它上面兩層不能是 symlink。
ba_root_ok() {
  for ba_q in "$HOME/.config" "$HOME/.config/agents-manager" "$ba_root"; do
    [ -L "$ba_q" ] && return 1
  done
  return 0
}
"#;

/// 只補不蓋的 TOML 合併：`AM_T`（表，空＝根層）裡沒有 `AM_K` 才補 `AM_K = AM_V`。
/// 結束碼 3＝鍵已在（不論值；`[表]` 裡的 `鍵 =`、根層的 `表.鍵 =` 或 `表 =` 行內表都算），不輸出。
/// 有 `[表]` 就補在表頭下一行；沒有就以點號鍵補在根層（第一個表頭之前，沒有表頭就接在最後）。
const TOML_AWK: &str = r#"
function keyname(l,   i, p) { i = index(l, "="); if (i == 0) return ""; p = substr(l, 1, i - 1); sub(/[[:blank:]]+$/, "", p); return p }
BEGIN { t = ENVIRON["AM_T"]; k = ENVIRON["AM_K"]; v = ENVIRON["AM_V"]; root = 1; intab = 0; found = 0; tabline = 0; first = 0 }
{
  lines[NR] = $0
  l = $0
  sub(/\r$/, "", l)
  sub(/^[[:blank:]]+/, "", l)
  if (l == "" || l ~ /^#/) next
  if (l ~ /^\[/) {
    if (!first) first = NR
    root = 0
    h = l
    sub(/\][[:blank:]]*(#.*)?$/, "]", h)
    gsub(/[[:blank:]]/, "", h)
    intab = (t != "" && h == "[" t "]")
    if (intab && !tabline) tabline = NR
    next
  }
  n = keyname(l)
  gsub(/[[:blank:]]/, "", n)
  if (t == "") { if (root && n == k) found = 1 }
  else if ((root && (n == t "." k || n == t)) || (intab && n == k)) found = 1
}
END {
  if (found) exit 3
  if (t != "" && tabline) at = tabline + 1
  else at = first ? first : NR + 1
  line = (t != "" && !tabline) ? t "." k " = " v : k " = " v
  for (i = 1; i <= NR; i++) {
    if (i == at) print line
    print lines[i]
  }
  if (at > NR) print line
}
"#;

const APPLY_SH: &str = r#"
printf 'AM_BA begin\n'
ba_end() { printf 'AM_BA end\n'; exit 0; }
case "$HOME" in /*) ;; *) printf 'AM_BA error no_home\n'; ba_end ;; esac
ba_awk='__AM_TOML_AWK__'
ba_jq=$(command -v jq 2>/dev/null || true)
case "$ba_jq" in /*) [ -x "$ba_jq" ] || ba_jq="" ;; *) ba_jq="" ;; esac
ba_dir=""
ba_seen="|"
ba_n=0
ba_stop=0
# 第一次真的要寫才建備份目錄；同一個檔只備份一次。manifest：每行 `file <編號> <原本在不在> <權限> <目標>`。
ba_backup() {
  case "$ba_seen" in *"|$1|"*) return 0 ;; esac
  if [ -z "$ba_dir" ]; then
    ba_root_ok || return 1
    mkdir -p "$ba_root" 2>/dev/null && chmod 700 "$ba_root" 2>/dev/null || return 1
    ba_new="$ba_root/$ba_id"
    mkdir "$ba_new" 2>/dev/null && mkdir "$ba_new/files" 2>/dev/null || return 1
    chmod 700 "$ba_new" "$ba_new/files" 2>/dev/null || return 1
    { printf 'am-baseline-backup 1\n'; printf 'id %s\n' "$ba_id"; printf 'plan_rev %s\n' "$ba_rev"; } > "$ba_new/manifest" 2>/dev/null || return 1
    chmod 600 "$ba_new/manifest" 2>/dev/null || return 1
    ba_dir=$ba_new
  fi
  ba_n=$((ba_n + 1))
  cp "$2" "$ba_dir/files/$ba_n" 2>/dev/null && chmod 600 "$ba_dir/files/$ba_n" 2>/dev/null || return 1
  printf 'file %s 1 %s %s\n' "$ba_n" "$(ba_mode "$2")" "$1" >> "$ba_dir/manifest" 2>/dev/null || return 1
  ba_seen="$ba_seen$1|"
}
ba_key() { printf 'AM_BA key %s %s\n' "$@"; }
# ba_one <編號> <目標> toml <表> <鍵> <值> ｜ ba_one <編號> <目標> json <點號路徑> <JSON 值>
ba_one() {
  ba_i=$1
  ba_tok=$2
  if [ "$ba_stop" = 1 ]; then ba_key "$ba_i" "skipped aborted"; return; fi
  ba_f=$(ba_path "$ba_tok") || { ba_key "$ba_i" "skipped bad_target"; return; }
  ba_d=$(dirname "$ba_f")
  if [ -L "$ba_f" ] || [ -L "$ba_d" ]; then ba_key "$ba_i" "skipped symlink"; return; fi
  [ -f "$ba_f" ] || { ba_key "$ba_i" "skipped file_missing"; return; }
  if [ "$3" = json ] && [ -z "$ba_jq" ]; then ba_key "$ba_i" "manual no_jq"; return; fi
  ba_tmp=$(mktemp "$ba_d/.am-baseline.XXXXXX" 2>/dev/null) || { ba_key "$ba_i" "skipped write_failed"; return; }
  if [ "$3" = toml ]; then
    AM_T=$4 AM_K=$5 AM_V=$6 /usr/bin/awk "$ba_awk" "$ba_f" > "$ba_tmp" 2>/dev/null
    ba_rc=$?
  else
    "$ba_jq" -e --arg p "$4" 'def bl_h($ks): if ($ks | length) == 0 then true elif type == "object" and has($ks[0]) then (.[$ks[0]] | bl_h($ks[1:])) else false end; bl_h($p | split("."))' "$ba_f" >/dev/null 2>&1
    case $? in
      0) ba_rc=3 ;;
      1) "$ba_jq" --arg p "$4" --argjson v "$5" 'setpath($p | split("."); $v)' "$ba_f" > "$ba_tmp" 2>/dev/null && ba_rc=0 || ba_rc=4 ;;
      *) ba_rc=5 ;;
    esac
  fi
  case $ba_rc in
    0) ;;
    3) rm -f "$ba_tmp"; ba_key "$ba_i" present; return ;;
    5) rm -f "$ba_tmp"; ba_key "$ba_i" "skipped invalid_json"; return ;;
    *) rm -f "$ba_tmp"; ba_key "$ba_i" "skipped conflict"; return ;;
  esac
  if ! ba_backup "$ba_tok" "$ba_f"; then
    rm -f "$ba_tmp"
    ba_stop=1
    printf 'AM_BA error backup_failed\n'
    ba_key "$ba_i" "skipped backup_failed"
    return
  fi
  ba_m=$(ba_mode "$ba_f")
  ba_sync "$ba_tmp"
  [ "$ba_m" = - ] || chmod "$ba_m" "$ba_tmp" 2>/dev/null
  if mv -f "$ba_tmp" "$ba_f" 2>/dev/null; then ba_key "$ba_i" applied; else rm -f "$ba_tmp"; ba_key "$ba_i" "skipped write_failed"; fi
}
"#;

/// 寫完才輪替：只算／只刪備份 id 形狀的真目錄，剛建的那份不刪。
const APPLY_TAIL_SH: &str = r#"
if [ -n "$ba_dir" ]; then
  ba_c=0
  for ba_x in "$ba_root"/[0-9]*T[0-9]*Z-*; do [ -d "$ba_x" ] && [ ! -L "$ba_x" ] && ba_c=$((ba_c + 1)); done
  for ba_x in "$ba_root"/[0-9]*T[0-9]*Z-*; do
    [ "$ba_c" -gt __AM_KEEP__ ] || break
    [ -d "$ba_x" ] && [ ! -L "$ba_x" ] || continue
    [ "$ba_x" = "$ba_dir" ] && continue
    rm -rf "$ba_x" && ba_c=$((ba_c - 1))
  done
  printf 'AM_BA backup\n'
fi
ba_end
"#;

/// 套用腳本（主機上 `/bin/sh`）。`backup_id` 必須是 [`valid_backup_id`]。輸出只有 `AM_BA` 標記行，不含路徑或內容。
pub fn render_apply_sh(backup_id: &str, plan_rev: &str, writes: &[KeyWrite]) -> Result<String, &'static str> {
    if !valid_backup_id(backup_id) {
        return Err("invalid_backup_id");
    }
    let mut s = String::new();
    s.push_str(COMMON_SH);
    s.push_str(&format!("ba_id={}\nba_rev={}\n", sh_quote(backup_id), sh_quote(plan_rev)));
    s.push_str(&APPLY_SH.replace("__AM_TOML_AWK__", TOML_AWK));
    for (i, w) in writes.iter().enumerate() {
        let args = match &w.format {
            Format::Toml { table, key, literal } => format!("toml {} {} {}", sh_quote(table), sh_quote(key), sh_quote(literal)),
            Format::Json { path, literal } => format!("json {} {}", sh_quote(path), sh_quote(literal)),
        };
        s.push_str(&format!("ba_one {i} {} {args}\n", sh_quote(&w.token)));
    }
    s.push_str(&APPLY_TAIL_SH.replace("__AM_KEEP__", &KEEP_BACKUPS.to_string()));
    Ok(s)
}

/// 套用腳本跑完的結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyRun {
    /// 每個寫入一筆，順序同 `writes`。
    pub outcomes: Vec<Outcome>,
    /// 這次有建備份（至少一個檔要寫）。
    pub backup_written: bool,
    /// 整趟的錯誤（`backup_failed`、`no_home`）。
    pub error: Option<String>,
}

fn reason_code(s: &str) -> &'static str {
    match s {
        "aborted" => "aborted",
        "bad_target" => "bad_target",
        "symlink" => "symlink",
        "file_missing" => "file_missing",
        "no_jq" => "no_jq",
        "write_failed" => "write_failed",
        "invalid_json" => "invalid_json",
        "conflict" => "conflict",
        "backup_failed" => "backup_failed",
        _ => "unknown",
    }
}

/// 解 [`render_apply_sh`] 的輸出。沒有 `AM_BA end`（被截斷）＝`None`；腳本沒回報的項目算 `skipped unknown`。
pub fn parse_apply(out: &str, writes: &[KeyWrite]) -> Option<ApplyRun> {
    if !out.lines().any(|l| l.trim_end() == "AM_BA end") {
        return None;
    }
    let mut outcomes: Vec<Outcome> =
        writes.iter().map(|w| Outcome { id: w.id.clone(), result: SKIPPED, reason: Some("unknown") }).collect();
    let (mut backup_written, mut error) = (false, None);
    for line in out.lines() {
        let f: Vec<&str> = line.trim_end().split(' ').collect();
        match f.as_slice() {
            ["AM_BA", "key", i, result, rest @ ..] => {
                let Some(o) = i.parse::<usize>().ok().and_then(|i| outcomes.get_mut(i)) else { continue };
                let (result, reason) = match *result {
                    "applied" => (APPLIED, None),
                    "present" => (PRESENT, None),
                    "manual" => (MANUAL, Some(reason_code(rest.first().copied().unwrap_or_default()))),
                    _ => (SKIPPED, Some(reason_code(rest.first().copied().unwrap_or_default()))),
                };
                o.result = result;
                o.reason = reason;
            }
            ["AM_BA", "backup"] => backup_written = true,
            ["AM_BA", "error", e] => error = Some(reason_code(e).to_string()),
            _ => {}
        }
    }
    Some(ApplyRun { outcomes, backup_written, error })
}

const RESTORE_SH: &str = r#"
printf 'AM_BR begin\n'
br_end() { printf 'AM_BR end\n'; exit 0; }
br_fail() { printf 'AM_BR error %s\n' "$1"; br_end; }
case "$HOME" in /*) ;; *) br_fail no_home ;; esac
br_dir="$ba_root/$br_id"
ba_root_ok || br_fail symlink
for br_q in "$br_dir" "$br_dir/files" "$br_dir/manifest"; do [ -L "$br_q" ] && br_fail symlink; done
[ -d "$br_dir" ] || br_fail not_found
[ -f "$br_dir/manifest" ] || br_fail manifest_invalid
set -f
# 第一趟：整份 manifest 驗過才動手。任一行不對＝整個拒絕，什麼都不寫。
br_hdr=0
br_count=0
br_seen="|"
while IFS= read -r br_l || [ -n "$br_l" ]; do
  if [ "$br_hdr" = 0 ]; then
    [ "$br_l" = "am-baseline-backup 1" ] || br_fail manifest_invalid
    br_hdr=1
    continue
  fi
  set -- $br_l
  case "$1" in
    id) [ $# -eq 2 ] && [ "$2" = "$br_id" ] || br_fail manifest_invalid ;;
    plan_rev) [ $# -eq 2 ] || br_fail manifest_invalid ;;
    file)
      [ $# -eq 5 ] || br_fail manifest_invalid
      case "$2" in ''|*[!0-9]*) br_fail manifest_invalid ;; esac
      case "$3" in 0|1) ;; *) br_fail manifest_invalid ;; esac
      case "$4" in -|[0-7][0-7][0-7]) ;; *) br_fail manifest_invalid ;; esac
      ba_token_ok "$5" || br_fail manifest_invalid
      case "$br_seen" in *"|$5|"*) br_fail manifest_invalid ;; esac
      br_seen="$br_seen$5|"
      if [ "$3" = 1 ]; then
        [ -L "$br_dir/files/$2" ] && br_fail symlink
        [ -f "$br_dir/files/$2" ] || br_fail manifest_invalid
      fi
      br_f=$(ba_path "$5") || br_fail manifest_invalid
      br_d=$(dirname "$br_f")
      if [ -L "$br_f" ] || [ -L "$br_d" ]; then br_fail symlink; fi
      [ -d "$br_d" ] || br_fail target_dir_missing
      if [ -e "$br_f" ] && [ ! -f "$br_f" ]; then br_fail not_a_file; fi
      br_count=$((br_count + 1))
      ;;
    *) br_fail manifest_invalid ;;
  esac
done < "$br_dir/manifest"
[ "$br_hdr" = 1 ] && [ "$br_count" -gt 0 ] || br_fail manifest_invalid
# 第二趟：照驗過的內容還原。原本在的檔：同目錄暫存檔＋sync＋原權限＋mv；原本不在的：刪掉後來建的。
br_hdr=0
while IFS= read -r br_l || [ -n "$br_l" ]; do
  if [ "$br_hdr" = 0 ]; then br_hdr=1; continue; fi
  set -- $br_l
  [ "$1" = file ] || continue
  br_f=$(ba_path "$5")
  if [ "$3" = 0 ]; then
    if [ -f "$br_f" ] && rm -f "$br_f" 2>/dev/null; then printf 'AM_BR file %s deleted\n' "$5"; else printf 'AM_BR file %s absent\n' "$5"; fi
    continue
  fi
  br_d=$(dirname "$br_f")
  br_tmp=$(mktemp "$br_d/.am-baseline.XXXXXX" 2>/dev/null) || { printf 'AM_BR file %s failed\n' "$5"; continue; }
  if cat "$br_dir/files/$2" > "$br_tmp" 2>/dev/null; then
    ba_sync "$br_tmp"
    [ "$4" = - ] || chmod "$4" "$br_tmp" 2>/dev/null
    if mv -f "$br_tmp" "$br_f" 2>/dev/null; then printf 'AM_BR file %s restored\n' "$5"; continue; fi
  fi
  rm -f "$br_tmp"
  printf 'AM_BR file %s failed\n' "$5"
done < "$br_dir/manifest"
br_end
"#;

/// 唯讀列出可還原備份。逐一驗完整 manifest 和 restore 所需的路徑／symlink 條件；不讀備份檔內容、不寫任何檔案。
const LIST_BACKUPS_SH: &str = r#"
printf 'AM_BK begin\n'
bk_end() { printf 'AM_BK skipped %s\n' "$bk_skipped"; printf 'AM_BK end\n'; exit 0; }
bk_skipped=0
bk_id_ok() {
  case "$1" in ????????T??????Z-????????-????-????-????-????????????) ;; *) return 1 ;; esac
  bk_stamp=${1%%-*}
  case "$bk_stamp" in *[!0123456789TZ]*) return 1 ;; esac
  bk_uuid=${1#????????T??????Z-}
  case "$bk_uuid" in *[!0123456789abcdef-]*) return 1 ;; esac
  return 0
}
case "$HOME" in /*) ;; *) bk_skipped=1; bk_end ;; esac
if ! ba_root_ok; then bk_skipped=1; bk_end; fi
if [ -L "$ba_root" ]; then bk_skipped=1; bk_end; fi
if [ ! -e "$ba_root" ]; then bk_end; fi
if [ ! -d "$ba_root" ]; then bk_skipped=1; bk_end; fi
for bk_dir in "$ba_root"/*; do
  [ -e "$bk_dir" ] || [ -L "$bk_dir" ] || continue
  bk_id=${bk_dir##*/}
  bk_id_ok "$bk_id" || continue
  bk_bad=0
  if [ -L "$bk_dir" ] || [ ! -d "$bk_dir" ]; then bk_bad=1; fi
  bk_files_dir="$bk_dir/files"
  bk_manifest="$bk_dir/manifest"
  if [ "$bk_bad" = 0 ] && { [ -L "$bk_files_dir" ] || [ ! -d "$bk_files_dir" ] || [ -L "$bk_manifest" ] || [ ! -f "$bk_manifest" ]; }; then bk_bad=1; fi
  bk_header=0
  bk_id_seen=0
  bk_plan_seen=0
  bk_count=0
  bk_seen="|"
  bk_file_ids="|"
  bk_meta=""
  if [ "$bk_bad" = 0 ]; then
    set -f
    while IFS= read -r bk_line || [ -n "$bk_line" ]; do
      if [ "$bk_header" = 0 ]; then
        [ "$bk_line" = "am-baseline-backup 1" ] || { bk_bad=1; break; }
        bk_header=1
        continue
      fi
      set -- $bk_line
      [ $# -gt 0 ] || { bk_bad=1; break; }
      case "$1" in
        id)
          [ $# -eq 2 ] && [ "$bk_id_seen" = 0 ] && [ "$2" = "$bk_id" ] || { bk_bad=1; break; }
          bk_id_seen=1
          ;;
        plan_rev)
          [ $# -eq 2 ] && [ "$bk_plan_seen" = 0 ] || { bk_bad=1; break; }
          bk_plan_seen=1
          ;;
        file)
          [ $# -eq 5 ] || { bk_bad=1; break; }
          case "$2" in ''|*[!0-9]*) bk_bad=1 ;; esac
          case "$3" in 0|1) ;; *) bk_bad=1 ;; esac
          case "$4" in -|[0-7][0-7][0-7]) ;; *) bk_bad=1 ;; esac
          ba_token_ok "$5" || bk_bad=1
          case "$bk_file_ids" in *"|$2|"*) bk_bad=1 ;; esac
          case "$bk_seen" in *"|$5|"*) bk_bad=1 ;; esac
          [ "$bk_bad" = 0 ] || break
          bk_file_ids="$bk_file_ids$2|"
          bk_seen="$bk_seen$5|"
          if [ "$3" = 1 ]; then
            [ -L "$bk_files_dir/$2" ] && bk_bad=1
            [ -f "$bk_files_dir/$2" ] || bk_bad=1
          fi
          bk_target=$(ba_path "$5") || bk_bad=1
          bk_target_dir=${bk_target%/*}
          if [ -L "$bk_target" ] || [ -L "$bk_target_dir" ] || [ ! -d "$bk_target_dir" ]; then bk_bad=1; fi
          if [ -e "$bk_target" ] && [ ! -f "$bk_target" ]; then bk_bad=1; fi
          [ "$bk_bad" = 0 ] || break
          if [ "$3" = 1 ]; then bk_action=overwrite; else bk_action=delete; fi
          bk_meta="$bk_meta"'AM_BK file '"$5"' '"$bk_action"'
'
          bk_count=$((bk_count + 1))
          ;;
        *) bk_bad=1; break ;;
      esac
    done < "$bk_manifest"
    set +f
  fi
  if [ "$bk_bad" = 0 ] && [ "$bk_header" = 1 ] && [ "$bk_id_seen" = 1 ] && [ "$bk_plan_seen" = 1 ] && [ "$bk_count" -gt 0 ]; then
    printf 'AM_BK backup %s\n' "$bk_id"
    printf '%s' "$bk_meta"
  else
    bk_skipped=$((bk_skipped + 1))
  fi
done
bk_end
"#;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BackupFile {
    /// 固定 allowlist 內的 manifest 目標名，不是主機路徑。
    pub file: String,
    /// `overwrite`＝備份時原檔存在；`delete`＝備份時原檔不存在，還原時會刪除。
    pub restore: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BackupSummary {
    pub backup_id: String,
    pub created_at: String,
    pub files: Vec<BackupFile>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BackupList {
    pub backups: Vec<BackupSummary>,
    pub skipped: usize,
}

/// 列舉腳本只讀備份目錄與 manifest；backup_id 在腳本拼路徑前先驗固定格式。
pub fn render_backup_list_sh() -> String {
    format!("{COMMON_SH}{LIST_BACKUPS_SH}")
}

/// 解列舉腳本標記；忽略 shell 的任何非協定輸出，目標名與結果再用 allowlist 驗一次。
pub fn parse_backup_list(out: &str) -> Option<BackupList> {
    if !out.lines().any(|l| l.trim_end() == "AM_BK end") {
        return None;
    }
    let mut backups: Vec<BackupSummary> = Vec::new();
    let mut current: Option<BackupSummary> = None;
    let mut protocol_skipped = None;
    let mut parser_skipped = 0usize;
    for line in out.lines() {
        let fields: Vec<_> = line.trim_end().split(' ').collect();
        match fields.as_slice() {
            ["AM_BK", "begin"] => {}
            ["AM_BK", "backup", id] => {
                if let Some(b) = current.take() {
                    if b.files.is_empty() {
                        parser_skipped = parser_skipped.saturating_add(1);
                    } else {
                        backups.push(b);
                    }
                }
                if !valid_backup_id(id) {
                    parser_skipped = parser_skipped.saturating_add(1);
                    continue;
                }
                let Some(created_at) = backup_created_at(id) else {
                    parser_skipped = parser_skipped.saturating_add(1);
                    continue;
                };
                current = Some(BackupSummary { backup_id: (*id).to_string(), created_at, files: Vec::new() });
            }
            ["AM_BK", "file", file, restore] => {
                let Some(b) = current.as_mut() else { continue };
                if !token_ok(file) || !matches!(*restore, "overwrite" | "delete") || b.files.iter().any(|f| f.file == *file) {
                    return None;
                }
                b.files.push(BackupFile { file: (*file).to_string(), restore: if *restore == "overwrite" { "overwrite" } else { "delete" } });
            }
            ["AM_BK", "skipped", n] => {
                protocol_skipped = Some(n.parse::<usize>().ok()?);
            }
            ["AM_BK", "end"] => {}
            ["AM_BK", ..] => return None,
            _ => {}
        }
    }
    if let Some(b) = current {
        if b.files.is_empty() {
            parser_skipped = parser_skipped.saturating_add(1);
        } else {
            backups.push(b);
        }
    }
    backups.sort_by(|a, b| b.backup_id.cmp(&a.backup_id));
    backups.dedup_by(|a, b| a.backup_id == b.backup_id);
    backups.truncate(KEEP_BACKUPS);
    Some(BackupList { backups, skipped: protocol_skipped?.saturating_add(parser_skipped) })
}

/// 還原腳本。`backup_id` 不符固定格式就不產生（API 回 400）。
pub fn render_restore_sh(backup_id: &str) -> Result<String, &'static str> {
    if !valid_backup_id(backup_id) {
        return Err("invalid_backup_id");
    }
    Ok(format!("{COMMON_SH}br_id={}\n{RESTORE_SH}", sh_quote(backup_id)))
}

/// 還原的一個檔：只有目標名與結果。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RestoredFile {
    /// 目標名（`codex/config.toml`…），不是主機上的路徑。
    pub file: String,
    /// `restored` | `deleted` | `absent` | `failed`
    pub result: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreRun {
    pub files: Vec<RestoredFile>,
    /// 整個拒絕的原因（`not_found`、`manifest_invalid`、`symlink`…）；有值時什麼都沒寫。
    pub error: Option<&'static str>,
}

/// 解 [`render_restore_sh`] 的輸出。沒有 `AM_BR end`＝`None`。目標名不在名單上的行丟掉（不回傳主機給的任意字串）。
pub fn parse_restore(out: &str) -> Option<RestoreRun> {
    if !out.lines().any(|l| l.trim_end() == "AM_BR end") {
        return None;
    }
    let mut run = RestoreRun { files: Vec::new(), error: None };
    for line in out.lines() {
        match line.trim_end().split(' ').collect::<Vec<_>>().as_slice() {
            ["AM_BR", "file", file, result] if token_ok(file) => {
                let result = match *result {
                    "restored" => "restored",
                    "deleted" => "deleted",
                    "absent" => "absent",
                    _ => "failed",
                };
                run.files.push(RestoredFile { file: file.to_string(), result });
            }
            ["AM_BR", "error", e] => {
                run.error = Some(match *e {
                    "not_found" => "not_found",
                    "symlink" => "symlink",
                    "target_dir_missing" => "target_dir_missing",
                    "not_a_file" => "not_a_file",
                    "no_home" => "no_home",
                    _ => "manifest_invalid",
                })
            }
            _ => {}
        }
    }
    Some(run)
}

/// 同腳本的 `ba_token_ok`。
fn token_ok(t: &str) -> bool {
    match t {
        "codex/config.toml" | "claude-default/settings.json" => true,
        _ => t
            .strip_prefix("claude-cc")
            .and_then(|r| r.strip_suffix("/settings.json"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())),
    }
}

#[cfg(test)]
mod tests {
    use super::super::plan::{self, FixAction};
    use super::*;
    use crate::hosts::remote_script_support::Scratch;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    const ID1: &str = "20261024T120000Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c4";

    fn act(id: &str, target: &str, key: &str, value: serde_json::Value) -> FixAction {
        FixAction { id: id.into(), action: MERGE_KEY, severity: "warn", target: target.into(), key: Some(key.into()), value: Some(value), reason: None }
    }

    fn hooks_plan() -> Plan {
        Plan { actions: vec![act("codex.config.toml:features.hooks", "codex/config.toml", "features.hooks", serde_json::json!(true))], plan_rev: "rev1".into() }
    }

    struct Home {
        _s: Scratch,
        h: PathBuf,
    }

    impl Home {
        fn new() -> Self {
            let s = Scratch::new();
            let h = s.0.join("home");
            std::fs::create_dir_all(&h).unwrap();
            Home { h, _s: s }
        }
        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let p = self.h.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
            p
        }
        fn read(&self, rel: &str) -> Vec<u8> {
            std::fs::read(self.h.join(rel)).unwrap()
        }
        fn run_path(&self, script: &str, path: &str) -> String {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .current_dir(&self.h)
                .env_clear()
                .env("HOME", &self.h)
                .env("PATH", path)
                .output()
                .unwrap();
            assert!(out.status.success(), "the script always exits 0: {out:?}");
            String::from_utf8(out.stdout).unwrap()
        }
        fn run(&self, script: &str) -> String {
            self.run_path(script, "/usr/bin:/bin")
        }
        fn apply(&self, id: &str, plan: &Plan) -> ApplyRun {
            let p = prepare(plan, None).unwrap();
            let out = self.run(&render_apply_sh(id, &plan.plan_rev, &p.writes).unwrap());
            parse_apply(&out, &p.writes).unwrap_or_else(|| panic!("no end marker: {out}"))
        }
        fn restore(&self, id: &str) -> RestoreRun {
            let out = self.run(&render_restore_sh(id).unwrap());
            parse_restore(&out).unwrap_or_else(|| panic!("no end marker: {out}"))
        }
        fn backups(&self) -> Vec<String> {
            let mut v: Vec<String> = match std::fs::read_dir(self.h.join(BACKUP_ROOT_REL)) {
                Ok(d) => d.map(|e| e.unwrap().file_name().into_string().unwrap()).collect(),
                Err(_) => vec![],
            };
            v.sort();
            v
        }
    }

    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
    }

    /// Same content/mode/mtime/ctime tree snapshot as host_baseline/tests.rs::snapshot.
    fn directory_snapshot(root: &Path) -> Vec<(String, Vec<u8>, u32, i64, i64)> {
        use std::os::unix::fs::MetadataExt;
        fn walk(p: &Path, root: &Path, acc: &mut Vec<(String, Vec<u8>, u32, i64, i64)>) {
            for entry in std::fs::read_dir(p).unwrap().flatten() {
                let path = entry.path();
                let metadata = entry.metadata().unwrap();
                let data = if metadata.is_file() { std::fs::read(&path).unwrap() } else { vec![] };
                acc.push((
                    path.strip_prefix(root).unwrap().display().to_string(),
                    data,
                    metadata.mode(),
                    metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
                    metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec(),
                ));
                if metadata.is_dir() {
                    walk(&path, root, acc);
                }
            }
        }
        let mut acc = Vec::new();
        walk(root, root, &mut acc);
        acc.sort();
        acc
    }

    fn id_n(n: usize) -> String {
        format!("20261024T12{n:04}Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c4")
    }

    #[test]
    fn backup_list_is_read_only_skips_bad_manifests_and_symlinks() {
        let home = Home::new();
        let root = home.h.join(BACKUP_ROOT_REL);
        let good = root.join(ID1);
        std::fs::create_dir_all(good.join("files")).unwrap();
        std::fs::create_dir_all(home.h.join(".codex")).unwrap();
        std::fs::create_dir_all(home.h.join(".claude")).unwrap();
        std::fs::write(home.h.join(".codex/config.toml"), "current\n").unwrap();
        std::fs::write(good.join("files/1"), "AM-SECRET-backup-content\n").unwrap();
        std::fs::write(
            good.join("manifest"),
            format!("am-baseline-backup 1\nid {ID1}\nplan_rev rev1\nfile 1 1 644 codex/config.toml\nfile 2 0 - claude-default/settings.json\n"),
        )
        .unwrap();

        let bad_id = "20261024T120001Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c5";
        let bad = root.join(bad_id);
        std::fs::create_dir_all(bad.join("files")).unwrap();
        std::fs::write(bad.join("manifest"), format!("am-baseline-backup 1\nid {bad_id}\nfile 1 1 600 /etc/passwd\n")).unwrap();
        let symlink_id = "20261024T120002Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c6";
        std::os::unix::fs::symlink(&good, root.join(symlink_id)).unwrap();

        let before = directory_snapshot(&home.h);
        let out = home.run(&render_backup_list_sh());
        let after = directory_snapshot(&home.h);
        assert_eq!(after, before, "listing must not write, delete, touch, or change file metadata");
        assert!(out.contains("AM_BK skipped 2\n"), "bad manifest and symlink must be counted: {out}");
        assert!(!out.contains("AM-SECRET") && !out.contains(home.h.to_str().unwrap()) && !out.contains(BACKUP_ROOT_REL), "{out}");
        assert!(out.lines().all(|line| line.starts_with("AM_BK ")), "unexpected output: {out}");
        assert_eq!(
            parse_backup_list(&out),
            Some(BackupList {
                backups: vec![BackupSummary {
                    backup_id: ID1.into(),
                    created_at: "2026-10-24T12:00:00Z".into(),
                    files: vec![
                        BackupFile { file: "codex/config.toml".into(), restore: "overwrite" },
                        BackupFile { file: "claude-default/settings.json".into(), restore: "delete" },
                    ],
                }],
                skipped: 2,
            })
        );
    }

    #[test]
    fn backup_list_refuses_a_symlinked_backup_root_without_following_it() {
        let home = Home::new();
        let outside = home.h.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(home.h.join(".config/agents-manager")).unwrap();
        std::os::unix::fs::symlink(&outside, home.h.join(BACKUP_ROOT_REL)).unwrap();
        let before = directory_snapshot(&home.h);
        let out = home.run(&render_backup_list_sh());
        assert_eq!(directory_snapshot(&home.h), before);
        assert!(!out.contains(outside.to_str().unwrap()));
        assert_eq!(parse_backup_list(&out).unwrap().skipped, 1);
    }

    #[test]
    fn backup_list_parser_rejects_unallowlisted_names_and_unknown_actions() {
        let header = format!("AM_BK begin\nAM_BK backup {ID1}\n");
        assert!(parse_backup_list(&format!("{header}AM_BK file /etc/passwd overwrite\nAM_BK skipped 0\nAM_BK end\n")).is_none());
        assert!(parse_backup_list(&format!("{header}AM_BK file codex/config.toml execute\nAM_BK skipped 0\nAM_BK end\n")).is_none());
        assert!(parse_backup_list(&format!("{header}AM_BK file codex/config.toml overwrite\nAM_BK end\n")).is_none());
    }

    #[test]
    fn backup_ids_have_one_fixed_shape() {
        let id = new_backup_id();
        assert!(valid_backup_id(&id), "{id}");
        assert_ne!(new_backup_id()[17..], id[17..]);
        assert!(valid_backup_id(ID1));
        for bad in [
            "",
            "../20261024T120000Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c4",
            "20261024T120000Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c/",
            "20261024T120000Z-0F8B2C1E-3A4D-4B5C-8D6E-7F8091A2B3C4",
            "20261024T120000Z-0f8b2c1e_3a4d-4b5c-8d6e-7f8091a2b3c4",
            "20261024X120000Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c4",
            "20261024T120000Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c4x",
            "20261024T120000Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c4a",
            "20261024T120000Z-01JABCDEF1234567890",
        ] {
            assert!(!valid_backup_id(bad), "{bad}");
            assert!(render_restore_sh(bad).is_err() && render_apply_sh(bad, "r", &[]).is_err(), "{bad}");
        }
    }

    /// 只拿計畫裡的 merge_key；指名不在裡面的 id＝錯；model 類鍵與 .gitconfig 再擋一次；write_file／工具不進來。
    #[test]
    fn prepare_takes_only_merge_keys_and_excludes_model_keys_and_secrets_again() {
        let mut p = hooks_plan();
        p.actions.push(FixAction { id: "codex.hooks.json".into(), action: plan::MANUAL, severity: "warn", target: "codex/hooks.json".into(), key: None, value: None, reason: Some(plan::NO_TEMPLATE) });
        p.actions.push(FixAction { id: "tool.rtk".into(), action: plan::MANUAL, severity: "critical", target: "tool/rtk".into(), key: None, value: None, reason: Some(plan::NO_INSTALLER) });
        // 繞過計畫產生器手造的：model 類鍵、gitconfig、自訂身份、巢狀值。
        p.actions.push(act("codex.config.toml:model", "codex/config.toml", "model", serde_json::json!("gpt-x")));
        p.actions.push(act("codex.config.toml:profiles.x.model", "codex/config.toml", "profiles.x.model", serde_json::json!("gpt-x")));
        p.actions.push(act("codex.config.toml:features.reasoning_effort", "codex/config.toml", "features.reasoning_effort", serde_json::json!("high")));
        p.actions.push(act("gitconfig.token", "git/.gitconfig", "url", serde_json::json!("x")));
        p.actions.push(act("claude.work.settings.json:statusLine", "claude:work/settings.json", "statusLine", serde_json::json!({"type": "command"})));
        p.actions.push(act("claude.cc2.settings.json:defaultMode", "claude:cc2/settings.json", "permissions.defaultMode", serde_json::json!("plan")));
        p.actions.push(act("codex.config.toml:a.b.c", "codex/config.toml", "a.b.c", serde_json::json!(1)));
        p.actions.push(act("codex.config.toml:approval_policy", "codex/config.toml", "approval_policy", serde_json::json!({"x": 1})));
        let got = prepare(&p, None).unwrap();
        let ids: Vec<&str> = got.writes.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, vec!["codex.config.toml:features.hooks", "claude.cc2.settings.json:defaultMode"]);
        assert_eq!(got.writes[1].token, "claude-cc2/settings.json");
        let manual: Vec<(&str, Option<&str>)> = got.manual.iter().map(|o| (o.id.as_str(), o.reason)).collect();
        assert_eq!(
            manual,
            vec![
                ("gitconfig.token", Some(plan::SECRET)),
                ("claude.work.settings.json:statusLine", Some("unsupported_identity")),
                ("codex.config.toml:a.b.c", Some("unsupported_key")),
                ("codex.config.toml:approval_policy", Some("unsupported_value")),
            ]
        );
        let script = render_apply_sh(ID1, "r", &got.writes).unwrap();
        for leaked in ["gpt-x", "high", "gitconfig", "hooks.json", "rtk", "profiles"] {
            assert!(!script.contains(leaked), "`{leaked}` reached the script");
        }
        assert_eq!(prepare(&p, Some(&["tool.rtk".to_string()])), Err("tool.rtk".to_string()), "manual items cannot be asked for");
        assert_eq!(prepare(&p, Some(&["nope".to_string()])), Err("nope".to_string()));
        let one = prepare(&p, Some(&["claude.cc2.settings.json:defaultMode".to_string()])).unwrap();
        assert_eq!(one.writes.len(), 1);
        assert!(one.manual.is_empty());
        assert!(!TOML_AWK.contains('\''), "the awk program is embedded in single quotes");
    }

    /// 缺鍵就補（寫前備份、manifest、權限）；已存在（不論值）不動、也不建備份；結果仍是合法 TOML。
    #[test]
    fn apply_adds_a_missing_key_after_a_backup_and_never_overwrites() {
        let home = Home::new();
        let orig = "approval_policy = \"on-request\"\n\n[features]\nother = 1\n\n[mcp_servers.x]\ncommand = \"y\"\n";
        let f = home.write(".codex/config.toml", orig);
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        let run = home.apply(ID1, &hooks_plan());
        assert_eq!(run.outcomes, vec![Outcome { id: "codex.config.toml:features.hooks".into(), result: APPLIED, reason: None }]);
        assert!((run.backup_written, run.error.clone()) == (true, None), "{run:?}");
        let now = String::from_utf8(home.read(".codex/config.toml")).unwrap();
        let v: toml::Value = toml::from_str(&now).unwrap_or_else(|e| panic!("{e}: {now}"));
        assert_eq!(v["features"]["hooks"].as_bool(), Some(true), "{now}");
        assert_eq!((v["features"]["other"].as_integer(), v["approval_policy"].as_str()), (Some(1), Some("on-request")));
        assert_eq!(now.replace("hooks = true\n", ""), orig, "only one line was added");
        assert_eq!(mode(&f), 0o640, "the original mode is kept");
        let b = home.h.join(BACKUP_ROOT_REL).join(ID1);
        assert_eq!((mode(&b), mode(&b.join("files")), mode(&b.join("files/1")), mode(&b.join("manifest"))), (0o700, 0o700, 0o600, 0o600));
        assert_eq!(mode(&home.h.join(BACKUP_ROOT_REL)), 0o700);
        assert_eq!(std::fs::read(b.join("files/1")).unwrap(), orig.as_bytes());
        let manifest = std::fs::read_to_string(b.join("manifest")).unwrap();
        assert_eq!(manifest, format!("am-baseline-backup 1\nid {ID1}\nplan_rev rev1\nfile 1 1 640 codex/config.toml\n"));
        let leftovers: Vec<_> = std::fs::read_dir(home.h.join(".codex")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "no temp file left behind: {leftovers:?}");

        // 第二次：鍵已在 → present，檔一個 byte 都不變，不建新備份。
        let before = home.read(".codex/config.toml");
        let run = home.apply(&id_n(1), &hooks_plan());
        assert_eq!((run.outcomes[0].result, run.backup_written), (PRESENT, false));
        assert_eq!(home.read(".codex/config.toml"), before);
        assert_eq!(home.backups(), vec![ID1.to_string()]);

        // 已存在但值不同（各種寫法）：一律不改。
        for existing in [
            "[features]\nhooks = false\n",
            "[features] # c\n  hooks=false # x\n",
            "features.hooks = false\n[other]\n",
            "features = { hooks = false }\n",
        ] {
            let home = Home::new();
            home.write(".codex/config.toml", existing);
            let run = home.apply(ID1, &hooks_plan());
            assert_eq!((run.outcomes[0].result, run.backup_written), (PRESENT, false), "{existing}");
            assert_eq!(home.read(".codex/config.toml"), existing.as_bytes(), "{existing}");
            assert!(home.backups().is_empty(), "nothing written, nothing backed up: {existing}");
        }
        // 沒有 [features]：以點號鍵補在根層（第一個表頭前），仍是合法 TOML。
        for (existing, want) in [
            ("model = \"m\"\n[profiles.a]\nx = 1\n", "model = \"m\"\nfeatures.hooks = true\n[profiles.a]\nx = 1\n"),
            ("a = 1", "a = 1\nfeatures.hooks = true\n"),
            ("[features.sub]\nx = 1\n", "features.hooks = true\n[features.sub]\nx = 1\n"),
        ] {
            let home = Home::new();
            home.write(".codex/config.toml", existing);
            let run = home.apply(ID1, &hooks_plan());
            assert_eq!(run.outcomes[0].result, APPLIED, "{existing}");
            let now = String::from_utf8(home.read(".codex/config.toml")).unwrap();
            assert_eq!(now, want);
            let v: toml::Value = toml::from_str(&now).unwrap_or_else(|e| panic!("{e}: {now}"));
            assert_eq!(v["features"]["hooks"].as_bool(), Some(true));
        }
        // 檔不在：不建。
        let home = Home::new();
        let run = home.apply(ID1, &hooks_plan());
        assert_eq!((run.outcomes[0].result, run.outcomes[0].reason), (SKIPPED, Some("file_missing")));
        assert!(!home.h.join(".codex/config.toml").exists() && home.backups().is_empty());
    }

    /// 目標、它的目錄、備份根目錄是 symlink：拒絕，連結後面的檔不動。
    #[test]
    fn apply_fails_closed_on_symlinks() {
        let home = Home::new();
        let outside = home.write("outside/config.toml", "a = 1\n");
        std::fs::create_dir_all(home.h.join(".codex")).unwrap();
        std::os::unix::fs::symlink(&outside, home.h.join(".codex/config.toml")).unwrap();
        let run = home.apply(ID1, &hooks_plan());
        assert_eq!((run.outcomes[0].result, run.outcomes[0].reason), (SKIPPED, Some("symlink")));
        assert_eq!(std::fs::read(&outside).unwrap(), b"a = 1\n");

        let home = Home::new();
        let outside = home.write("elsewhere/config.toml", "a = 1\n");
        std::os::unix::fs::symlink(home.h.join("elsewhere"), home.h.join(".codex")).unwrap();
        let run = home.apply(ID1, &hooks_plan());
        assert_eq!(run.outcomes[0].reason, Some("symlink"));
        assert_eq!(std::fs::read(&outside).unwrap(), b"a = 1\n");

        // 備份根目錄是 symlink：備份失敗 → 整趟停、檔不動。
        let home = Home::new();
        home.write(".codex/config.toml", "a = 1\n");
        std::fs::create_dir_all(home.h.join("bk")).unwrap();
        std::fs::create_dir_all(home.h.join(".config/agents-manager")).unwrap();
        std::os::unix::fs::symlink(home.h.join("bk"), home.h.join(BACKUP_ROOT_REL)).unwrap();
        let run = home.apply(ID1, &hooks_plan());
        assert_eq!((run.outcomes[0].reason, run.error.as_deref(), run.backup_written), (Some("backup_failed"), Some("backup_failed"), false));
        assert_eq!(home.read(".codex/config.toml"), b"a = 1\n");
        assert_eq!(std::fs::read_dir(home.h.join("bk")).unwrap().count(), 0);
    }

    /// 還原後逐 byte 相同、權限也回來；備份留著。
    #[test]
    fn restore_brings_back_the_exact_bytes() {
        let home = Home::new();
        let orig = "# c\r\napproval_policy = \"x\"\n[features]\nother = true\n";
        let f = home.write(".codex/config.toml", orig);
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(home.apply(ID1, &hooks_plan()).outcomes[0].result, APPLIED);
        assert_ne!(home.read(".codex/config.toml"), orig.as_bytes());
        let run = home.restore(ID1);
        assert_eq!(run, RestoreRun { files: vec![RestoredFile { file: "codex/config.toml".into(), result: "restored" }], error: None });
        assert_eq!(home.read(".codex/config.toml"), orig.as_bytes());
        assert_eq!(mode(&f), 0o644);
        assert_eq!(home.backups(), vec![ID1.to_string()]);
        assert_eq!(home.restore(&id_n(9)).error, Some("not_found"));
    }

    /// 主機上的 manifest 是不可信輸入：任一行不對整個拒絕，連前面合法的那行也不還原。
    #[test]
    fn restore_rejects_an_untrusted_manifest_as_a_whole() {
        let modified = b"features.hooks = true\n";
        let cases: Vec<(&str, Box<dyn Fn(&Path, &Path)>)> = vec![
            ("absolute", Box::new(|_, _| {})),
            ("dotdot", Box::new(|_, _| {})),
            ("other file", Box::new(|_, _| {})),
            ("bad header", Box::new(|_, _| {})),
            ("other id", Box::new(|_, _| {})),
            ("unknown line", Box::new(|_, _| {})),
            ("bad mode", Box::new(|_, _| {})),
            ("duplicate", Box::new(|_, _| {})),
            ("backup is a symlink", Box::new(|b, h| {
                std::fs::write(h.join("secret"), "S").unwrap();
                std::fs::remove_file(b.join("files/1")).unwrap();
                std::os::unix::fs::symlink(h.join("secret"), b.join("files/1")).unwrap();
            })),
            ("target is a symlink", Box::new(|_, h| {
                std::fs::write(h.join("t"), "T").unwrap();
                std::os::unix::fs::symlink(h.join("t"), h.join(".claude-cc1/settings.json")).unwrap();
            })),
            ("manifest is a symlink", Box::new(|b, h| {
                std::fs::rename(b.join("manifest"), h.join("m")).unwrap();
                std::os::unix::fs::symlink(h.join("m"), b.join("manifest")).unwrap();
            })),
        ];
        for (name, tweak) in cases {
            let home = Home::new();
            home.write(".codex/config.toml", std::str::from_utf8(modified).unwrap());
            std::fs::create_dir_all(home.h.join(".claude-cc1")).unwrap();
            let b = home.h.join(BACKUP_ROOT_REL).join(ID1);
            std::fs::create_dir_all(b.join("files")).unwrap();
            std::fs::write(b.join("files/1"), "ORIGINAL\n").unwrap();
            std::fs::write(b.join("files/2"), "{}\n").unwrap();
            let good = "file 1 1 600 codex/config.toml\n";
            let second = match name {
                "absolute" => "file 2 1 600 /etc/passwd\n".to_string(),
                "dotdot" => "file 2 1 600 claude-cc1/../../.ssh/settings.json\n".into(),
                "other file" => "file 2 1 600 .ssh/id_rsa\n".into(),
                "unknown line" => "exec rm -rf /\n".into(),
                "bad mode" => "file 2 1 7777 claude-cc1/settings.json\n".into(),
                "duplicate" => good.into(),
                _ => "file 2 1 600 claude-cc1/settings.json\n".into(),
            };
            let header = if name == "bad header" { "am-baseline-backup 2\n" } else { "am-baseline-backup 1\n" };
            let id = if name == "other id" { id_n(3) } else { ID1.to_string() };
            std::fs::write(b.join("manifest"), format!("{header}id {id}\n{good}{second}")).unwrap();
            tweak(&b, &home.h);
            // 逃出去的目標名真的指得到的目錄也先建好：擋下來的必須是名單，不是「目錄不在」。
            std::fs::create_dir_all(home._s.0.join(".ssh")).unwrap();
            let want = if name.contains("symlink") { "symlink" } else { "manifest_invalid" };
            let run = home.restore(ID1);
            assert_eq!((run.error, run.files.len()), (Some(want), 0), "{name}: {run:?}");
            assert_eq!(home.read(".codex/config.toml"), modified, "{name}: nothing may be restored");
            assert!(!home.h.join("etc").exists() && !home.h.join(".ssh").exists(), "{name}");
        }
    }

    /// manifest 說原本沒有的檔：還原＝刪掉後來建的（3a 本身不建檔，這條守的是 restore 的語意）。
    #[test]
    fn restore_deletes_a_file_that_did_not_exist_before() {
        let home = Home::new();
        home.write(".claude/settings.json", "{\"x\":1}\n");
        let b = home.h.join(BACKUP_ROOT_REL).join(ID1);
        std::fs::create_dir_all(b.join("files")).unwrap();
        std::fs::write(b.join("manifest"), format!("am-baseline-backup 1\nid {ID1}\nfile 1 0 - claude-default/settings.json\n")).unwrap();
        let run = home.restore(ID1);
        assert_eq!(run.files, vec![RestoredFile { file: "claude-default/settings.json".into(), result: "deleted" }]);
        assert!(!home.h.join(".claude/settings.json").exists());
    }

    /// 只留最新 10 份：最舊的先刪，剛建的那份一定留；不是備份 id 形狀的東西不碰。
    #[test]
    fn backups_rotate_to_the_newest_ten() {
        let home = Home::new();
        std::fs::create_dir_all(home.h.join(BACKUP_ROOT_REL).join("keep-me")).unwrap();
        for n in 1..=12 {
            home.write(".codex/config.toml", "a = 1\n");
            let run = home.apply(&id_n(n), &hooks_plan());
            assert_eq!(run.outcomes[0].result, APPLIED, "{n}");
        }
        let mut want: Vec<String> = (3..=12).map(id_n).collect();
        want.push("keep-me".into());
        assert_eq!(home.backups(), want);
    }

    /// settings.json：有 jq 就以 jq 補（已存在不動）；沒有 jq 就降級 manual，一個字都不寫。
    #[test]
    fn json_keys_need_jq_and_degrade_to_manual_without_it() {
        let plan = Plan {
            actions: vec![act("claude.cc1.settings.json:defaultMode", "claude:cc1/settings.json", "permissions.defaultMode", serde_json::json!("plan"))],
            plan_rev: "r".into(),
        };
        let body = "{\"permissions\":{\"allow\":[\"x\"]}}\n";
        let p = prepare(&plan, None).unwrap();
        let script = render_apply_sh(ID1, "r", &p.writes).unwrap();

        // 沒有 jq 的 PATH：只放腳本用得到的工具。
        let home = Home::new();
        home.write(".claude-cc1/settings.json", body);
        let bin = home.h.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for t in ["mkdir", "chmod", "cp", "mv", "rm", "ls", "cut", "dirname", "mktemp", "sync", "cat"] {
            let src = ["/bin", "/usr/bin"].iter().map(|d| Path::new(d).join(t)).find(|p| p.exists()).unwrap();
            std::os::unix::fs::symlink(src, bin.join(t)).unwrap();
        }
        let out = home.run_path(&script, bin.to_str().unwrap());
        let run = parse_apply(&out, &p.writes).unwrap();
        assert_eq!((run.outcomes[0].result, run.outcomes[0].reason), (MANUAL, Some("no_jq")), "{out}");
        assert_eq!(home.read(".claude-cc1/settings.json"), body.as_bytes());
        assert!(home.backups().is_empty());

        if !Path::new("/usr/bin/jq").exists() && !Path::new("/bin/jq").exists() {
            return;
        }
        let home = Home::new();
        home.write(".claude-cc1/settings.json", body);
        let run = parse_apply(&home.run(&script), &p.writes).unwrap();
        assert_eq!(run.outcomes[0].result, APPLIED);
        let v: serde_json::Value = serde_json::from_slice(&home.read(".claude-cc1/settings.json")).unwrap();
        assert_eq!(v, serde_json::json!({"permissions": {"allow": ["x"], "defaultMode": "plan"}}));
        assert_eq!(home.restore(ID1).files[0].result, "restored");
        assert_eq!(home.read(".claude-cc1/settings.json"), body.as_bytes());
        for (existing, want) in [("{\"permissions\":{\"defaultMode\":\"ask\"}}", PRESENT), ("{\"permissions\":\"x\"}", SKIPPED), ("{ nope", SKIPPED)] {
            let home = Home::new();
            home.write(".claude-cc1/settings.json", existing);
            let run = parse_apply(&home.run(&script), &p.writes).unwrap();
            assert_eq!(run.outcomes[0].result, want, "{existing}");
            assert_eq!(home.read(".claude-cc1/settings.json"), existing.as_bytes());
        }
    }

    /// 腳本輸出與解析結果都不帶主機路徑或內容；restore 的目標名不在名單上就丟掉。
    #[test]
    fn outputs_carry_no_paths_or_contents() {
        let home = Home::new();
        home.write(".codex/config.toml", "approval_policy = \"AM-SECRET\"\n");
        let p = prepare(&hooks_plan(), None).unwrap();
        let out = home.run(&render_apply_sh(ID1, "rev1", &p.writes).unwrap());
        let rout = home.run(&render_restore_sh(ID1).unwrap());
        for text in [&out, &rout] {
            assert!(!text.contains("AM-SECRET") && !text.contains(home.h.to_str().unwrap()) && !text.contains("baseline-backup"), "{text}");
            assert!(text.lines().all(|l| l.starts_with("AM_BA ") || l.starts_with("AM_BR ")), "{text}");
        }
        let r = parse_restore("AM_BR file /etc/passwd restored\nAM_BR file codex/config.toml weird\nAM_BR end\n").unwrap();
        assert_eq!(r.files, vec![RestoredFile { file: "codex/config.toml".into(), result: "failed" }]);
        assert!(parse_apply("AM_BA key 0 applied\n", &p.writes).is_none(), "no end marker = unknown");
        assert!(parse_restore("AM_BR file codex/config.toml restored\n").is_none());
    }

    /// 真探測（PROBE_SH＋一致性檢查）在有備份目錄的假 HOME 裡跑：輸出不含備份路徑、id 或內容。
    #[test]
    fn the_real_probe_never_reads_the_backup_dir() {
        let home = Home::new();
        home.write(".codex/config.toml", "a = 1\n");
        let b = home.h.join(BACKUP_ROOT_REL).join(ID1);
        std::fs::create_dir_all(b.join("files")).unwrap();
        std::fs::write(b.join("files/1"), "AM-BACKUP-CONTENT\n").unwrap();
        std::fs::write(b.join("manifest"), format!("am-baseline-backup 1\nid {ID1}\nfile 1 1 600 codex/config.toml\n")).unwrap();
        let script = format!("{}\n{}", crate::tools::PROBE_SH, super::super::baseline_sh());
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .current_dir(&home.h)
            .env_clear()
            .env("HOME", &home.h)
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .output()
            .unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(text.contains("AM_BL end"), "the probe ran to the end: {text}");
        for needle in ["baseline-backup", "AM-BACKUP-CONTENT", ID1, "agents-manager/"] {
            assert!(!text.contains(needle), "`{needle}` leaked into the probe output: {text}");
        }
    }

    /// `scripts/ops/host-baseline-sync-mac-verify.sh` 內嵌腳本必須與目前產生的版本一致（改了腳本就要更新它，再到 Mac 上跑一次）。
    #[test]
    fn the_mac_verify_script_embeds_the_current_scripts() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/ops/host-baseline-sync-mac-verify.sh");
        let text = std::fs::read_to_string(&path).unwrap();
        let embedded = |tag: &str| {
            let start = format!("<<'{tag}'\n");
            let from = text.find(&start).unwrap_or_else(|| panic!("no {tag} heredoc")) + start.len();
            let len = text[from..].find(&format!("\n{tag}\n")).unwrap_or_else(|| panic!("{tag} heredoc not closed"));
            text[from..from + len + 1].to_string()
        };
        let writes = prepare(&hooks_plan(), None).unwrap().writes;
        let apply = render_apply_sh(MAC_VERIFY_ID, "macverify", &writes).unwrap();
        let restore = render_restore_sh(MAC_VERIFY_ID).unwrap();
        let backups = render_backup_list_sh();
        assert!(embedded("AM_APPLY_SH") == apply, "apply script drifted: paste the current render_apply_sh output into the heredoc and re-verify on a Mac");
        assert!(embedded("AM_RESTORE_SH") == restore, "restore script drifted: paste the current render_restore_sh output into the heredoc and re-verify on a Mac");
        assert!(embedded("AM_LIST_SH") == backups, "backup list script drifted: paste the current render_backup_list_sh output into the heredoc and re-verify on a Mac");
        assert!(apply.lines().filter(|l| l.starts_with("ba_id=")).count() == 1 && restore.lines().filter(|l| l.starts_with("br_id=")).count() == 1);
    }

    const MAC_VERIFY_ID: &str = "20261024T000000Z-00000000-0000-4000-8000-000000000000";

    /// 還原只認宣告檔裡的那兩種檔。
    #[test]
    fn restorable_targets_are_declared_in_the_manifest() {
        let m = super::super::manifest::shipped().unwrap();
        assert!(m.codex_files.iter().any(|f| f.name == "config.toml"));
        assert!(m.claude_files.iter().any(|f| f.name == "settings.json"));
        for t in ["codex/config.toml", "claude-default/settings.json", "claude-cc12/settings.json"] {
            assert!(token_ok(t), "{t}");
        }
        for t in ["claude-cc/settings.json", "claude-ccx/settings.json", "codex/hooks.json", "claude-cc1/../x/settings.json", ".gitconfig"] {
            assert!(!token_ok(t), "{t}");
        }
    }
}
