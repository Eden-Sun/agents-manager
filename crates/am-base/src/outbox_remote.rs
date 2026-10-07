//! 遠端主機 bot 的 outbox（SPEC §6.5f，使用者 2026-10-01「想辦法解決」）。
//!
//! 以前遠端 bot 沒有 `AM_OUTBOX`，網頁只說「檔案不在這台機器，列不出來」。現在遠端 pane 也拿到 `AM_OUTBOX`，
//! 指到**那台主機上**的 `~/<remote root>/outbox/<bot_id>/`（跟 bot 目錄同一個實例根），網頁列表與下載走 ssh：
//!
//! - 只列 outbox 最上層的一般檔（`find -type f` 的語意：符號連結不算）；outbox 路徑任一元件是符號連結就整個不列。
//! - 跟本機同一套擋法：`outbox::withheld_name`（私鑰／憑證／DB／隱藏檔）不列也不給，內容開頭像私鑰或 SQLite 的也一樣。
//! - 下載只收單一層檔名（沒有 `/`、不以 `.` 開頭），大小上限同本機；內容用 base64 傳回來，二進位檔不會被 UTF-8 弄壞。
//! - 遠端沒有 AGM 的 `outbox-gc`：每次列表時順手刪掉超過一小時（[`crate::outbox::TTL_SECS`]，mtime 與 ctime 都要過）的檔，跟本機的承諾一致。

use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use serde_json::json;

use crate::hosts::{sh_quote, HostConn};
use crate::lc_error::LcError;
use crate::outbox::{content_is_withheld, content_disposition, mime_of, withheld_name, MAX_BYTES, MAX_ENTRIES, TTL_SECS};

/// 下載大檔走 base64 會比較久；列表很快。
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const FILE_TIMEOUT: Duration = Duration::from_secs(180);

/// 遠端那台上這顆 bot 的 outbox（絕對路徑）。bot id 只收英數（ULID），跟本機 [`crate::outbox::dir_for`] 同一條。
pub fn remote_dir(home: &str, instance: Option<&str>, bot_id: &str) -> Option<String> {
    if bot_id.is_empty()
        || !bot_id.chars().all(|c| c.is_ascii_alphanumeric())
        || !home.starts_with('/')
        || home.chars().any(char::is_control)
    {
        return None;
    }
    let mut parts = Vec::new();
    for part in home.split('/') {
        if part.is_empty() {
            continue;
        }
        if part == "." || part == ".." {
            return None;
        }
        parts.push(part);
    }
    let normalized_home = if parts.is_empty() { "/".to_string() } else { format!("/{}", parts.join("/")) };
    let prefix = if normalized_home == "/" { "" } else { &normalized_home };
    Some(format!("{prefix}/{}/outbox/{bot_id}", crate::hosts::remote_root_for(instance)))
}

pub struct Target {
    conn: Arc<HostConn>,
    dir: String,
}

#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn target_for_test(conn: Arc<HostConn>, dir: impl Into<String>) -> Target {
    Target { conn, dir: dir.into() }
}

fn unreachable_body(host: &str) -> serde_json::Value {
    json!({"reason": "outbox_remote_unreachable", "host": host})
}

fn unreachable(host: &str) -> LcError {
    LcError::conflict("outbox_remote_unreachable", unreachable_body(host))
}

/// 遠端 outbox 還要：那台的連線、這顆 daemon 的實例 slug（`App` 在 `app_ports_p10` 實作）。
pub trait OutboxRemoteEnv: crate::outbox::OutboxEnv {
    fn host_conn(&self, host: &str) -> impl std::future::Future<Output = Option<Arc<HostConn>>> + Send;
    fn instance(&self) -> Option<String>;
}

impl<T: OutboxRemoteEnv + ?Sized> OutboxRemoteEnv for Arc<T> {
    fn host_conn(&self, host: &str) -> impl std::future::Future<Output = Option<Arc<HostConn>>> + Send {
        (**self).host_conn(host)
    }
    fn instance(&self) -> Option<String> {
        (**self).instance()
    }
}

/// 這顆 bot 在遠端主機上就回它的 outbox；本機 bot 回 `None`（照舊走 [`crate::outbox`]）。
pub(crate) async fn target(app: &impl OutboxRemoteEnv, bot_id: &str) -> Result<Option<Target>, LcError> {
    let project = app.bot_place(bot_id).await.map_err(|_| LcError::NotFound("bot".into()))?;
    if project.host == crate::config::LOCAL_HOST {
        return Ok(None);
    }
    let conn = app.host_conn(&project.host).await.ok_or_else(|| unreachable(&project.host))?;
    // 已知連不上（睡著、tailscale 斷線）就直接說連不上：不去等一趟 ssh（列表 30 秒、下載 180 秒才逾時），
    // 網頁每開一次也不會多養一條卡住的 ssh 行程。
    if !conn.is_connected() {
        return Err(unreachable(&project.host));
    }
    let home = conn.home().await.map_err(|_| unreachable(&project.host))?;
    let dir = remote_dir(&home, app.instance().as_deref(), bot_id).ok_or_else(|| LcError::NotFound("bot".into()))?;
    Ok(Some(Target { conn, dir }))
}

/// 兩條遠端路徑共用的 identity 與 link-count 檢查。除 inode 外也確認 fd 解析出的實際檔名仍是 outbox 內指定項目，
/// 避免在 `-L` 檢查後換回 symlink 時只靠路徑與 fd 比對而讀到外部檔。GNU Linux 讀 `/proc/self/fd`；BSD/macOS 用 lsof。
pub fn file_identity_helpers(lsof_path: &str) -> String {
    file_identity_helpers_with_paths(lsof_path, "/usr/bin/lsof")
}

pub fn file_identity_helpers_with_paths(lsof_path: &str, fallback_lsof_path: &str) -> String {
    let template = r#"am_same() {
  expected=$(pwd -P)/${F#./}
  if [ -n "$G" ]; then
    actual=$(readlink "/proc/self/fd/3") || return 1
    [ "$actual" = "$expected" ] && [ "$F" -ef /dev/fd/3 ]
    return $?
  fi
  exec 4< "$F" || return 1
  if ! am_dir_same || [ -L "$F" ] || [ ! -f "$F" ]; then exec 4<&-; return 1; fi
  lsof_bin=__AM_LSOF_PATH__
  [ -x "$lsof_bin" ] || lsof_bin=__AM_LSOF_FALLBACK__
  [ -x "$lsof_bin" ] || { exec 4<&-; return 1; }
  lsof_out=$("$lsof_bin" -a -p "$$" -d 3,4 -FfDin 2>/dev/null) || { exec 4<&-; return 1; }
  ids=$(printf '%s\n' "$lsof_out" | /usr/bin/awk -v expected="$expected" '
    /^f3$/ || /^f3[^0-9]/ { if (seen3++) bad=1; fd=3; next }
    /^f4$/ || /^f4[^0-9]/ { if (seen4++) bad=1; fd=4; next }
    /^f[0-9]/ { bad=1; fd=0; next }
    /^D/ && fd { if (seen_dev[fd]++) bad=1; dev[fd]=substr($0,2); next }
    /^i/ && fd { if (seen_ino[fd]++) bad=1; ino[fd]=substr($0,2); next }
    /^n/ && fd { if (seen_name[fd]++) bad=1; name[fd]=substr($0,2); next }
    END {
      if (seen3 == 1 && seen4 == 1 && !bad && seen_dev[3] == 1 && seen_ino[3] == 1 && seen_name[3] == 1 &&
          seen_dev[4] == 1 && seen_ino[4] == 1 && seen_name[4] == 1 &&
          dev[3] ~ /^0x[[:xdigit:]]+$/ && dev[4] ~ /^0x[[:xdigit:]]+$/ && ino[3] ~ /^[0-9]+$/ && ino[4] ~ /^[0-9]+$/ &&
          dev[3] == dev[4] && ino[3] == ino[4] && name[3] == expected && name[4] == expected) print "same"
      else exit 1
    }')
  ok=$?
  exec 4<&-
  [ "$ok" -eq 0 ] && [ "$ids" = same ]
}
am_singlelink() {
  if [ -n "$G" ]; then l=$(stat -L -c '%h' /dev/fd/3 2>/dev/null) || return 1
  else l=$(stat -L -f '%l' /dev/fd/3 2>/dev/null) || return 1; fi
  [ "$l" = 1 ]
}
"#;
    template
        .replace("__AM_LSOF_PATH__", &sh_quote(lsof_path))
        .replace("__AM_LSOF_FALLBACK__", &sh_quote(fallback_lsof_path))
}

/// Resolve the remote outbox once, reject symlinked path components, then use the shell's pinned
/// working directory for all subsequent relative opens. This avoids re-resolving `$D` after checks.
pub fn directory_identity_helpers() -> &'static str {
    r#"am_enter_dir() {
  case "$D" in /*) ;; *) return 1 ;; esac
  CDPATH= cd -P / 2>/dev/null || return 1
  rest=${D#/}
  physical=/
  while [ -n "$rest" ]; do
    case "$rest" in
      */*) part=${rest%%/*}; rest=${rest#*/} ;;
      *) part=$rest; rest= ;;
    esac
    [ -n "$part" ] || continue
    if [ ! -d "$part" ]; then
      if [ -e "$part" ] || [ -L "$part" ]; then return 1; else return 2; fi
    fi
    [ ! -L "$part" ] || return 1
    if [ "$physical" = "/" ]; then expected="/$part"; else expected="$physical/$part"; fi
    CDPATH= cd -P "$part" 2>/dev/null || return 1
    physical=$(pwd -P) || return 1
    [ "$physical" = "$expected" ] || return 1
  done
  [ "$D" -ef . ]
}
am_dir_same() {
  [ ! -L "$D" ] && [ "$D" -ef . ]
}
"#
}

pub fn list_script(dir: &str) -> String {
    list_script_race(dir, "", "")
}

fn list_script_race(dir: &str, before_open: &str, after_open: &str) -> String {
    list_script_race_with_lsof(dir, before_open, after_open, "/usr/sbin/lsof")
}

pub fn list_script_race_with_lsof(dir: &str, before_open: &str, after_open: &str, lsof_path: &str) -> String {
    format!(
        r#"D={d}
{dir_helpers}
am_enter_dir
enter_status=$?
case "$enter_status" in
  0) ;;
  2) printf 'AM_OUTBOX_OK\nAM_OUTBOX_DONE\n'; exit 0 ;;
  *) printf 'AM_OUTBOX_UNTRUSTED\n'; exit 0 ;;
esac
if ! find . -maxdepth 1 -type f -mmin +{ttl_min} -cmin +{ttl_min} -exec rm -f {{}} + 2>/dev/null; then
  printf 'AM_OUTBOX_UNTRUSTED\n'
  exit 0
fi
if stat -c %Y . >/dev/null 2>&1; then G=1; else G=; fi
printf 'AM_OUTBOX_OK\n'
am_list_entry() {{
  exec 3<&-
  exec 3< "$F" || return 0
  {after_open}
  if ! am_dir_same || [ -L "$F" ] || [ ! -f "$F" ]; then exec 3<&-; return 0; fi
  {helpers}
  if ! am_same; then exec 3<&-; return 0; fi
  if [ -n "$G" ]; then
    m=$(stat -L -c '%s %Y %Z' /dev/fd/3) || {{ exec 3<&-; return 0; }}
  else
    m=$(stat -L -f '%z %m %c' /dev/fd/3) || {{ exec 3<&-; return 0; }}
  fi
  if ! am_singlelink; then exec 3<&-; return 0; fi
  h=$(head -c 64 <&3 | od -An -tx1 | tr -d ' \n')
  printf '%s\t%s\t%s\n' "$m" "$h" "$N"
  exec 3<&-
}}
for f in ./*; do
  [ -f "$f" ] && [ ! -L "$f" ] || continue
  N=${{f##*/}}
  case "$N" in *[[:cntrl:]]*) continue;; esac
  F="$f"
  {before_open}
  am_list_entry 2>/dev/null
done
printf 'AM_OUTBOX_DONE\n'
"#,
        d = sh_quote(dir),
        ttl_min = TTL_SECS / 60,
        before_open = before_open,
        after_open = after_open,
        helpers = file_identity_helpers(lsof_path),
        dir_helpers = directory_identity_helpers(),
    )
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()).collect()
}

/// `list_script` 的輸出 → 跟本機 `outbox::scan` 同形狀的清單。`None`＝不可信或未完整列舉。
pub fn parse_list(out: &str, now: u64) -> Option<Vec<serde_json::Value>> {
    let payload = out.strip_prefix("AM_OUTBOX_OK\n")?.strip_suffix("AM_OUTBOX_DONE\n")?;
    let mut files: Vec<(String, u64, u64, u64)> = Vec::new();
    for line in payload.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(meta), Some(hex), Some(name)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let mut meta = meta.split_whitespace();
        let (Some(Ok(size)), Some(Ok(modified))) = (meta.next().map(str::parse::<u64>), meta.next().map(str::parse::<u64>)) else { continue };
        // 第三欄 ctime＝搬進來的時間；舊格式沒有這一欄就只看 mtime。
        let changed = meta.next().and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
        if name.is_empty()
            || name.contains('/')
            || name.chars().any(char::is_control)
            || withheld_name(&name.to_ascii_lowercase())
            || content_is_withheld(&hex_bytes(hex))
        {
            continue;
        }
        files.push((name.to_string(), size, modified, modified.max(changed)));
    }
    files.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
    files.truncate(MAX_ENTRIES);
    Some(
        files
            .into_iter()
            .map(|(name, size, modified, landed)| {
                let expires_at = landed + TTL_SECS;
                json!({"name": name, "size": size, "modified": modified, "expires_at": expires_at, "remaining_secs": expires_at.saturating_sub(now)})
            })
            .collect(),
    )
}

pub async fn list(t: Target, now: u64) -> Result<Response, LcError> {
    // `target()` and I/O are separate awaits. The supervisor can mark this connection down
    // between them, so don't start a new SSH process after that state is already known.
    if !t.conn.is_connected() {
        return Ok((StatusCode::OK, axum::Json({
            let mut body = unreachable_body(&t.conn.name);
            body["files"] = json!([]);
            body["ttl_secs"] = json!(TTL_SECS);
            body
        })).into_response());
    }
    let body = match t.conn.ssh_exec_timeout(&list_script(&t.dir), LIST_TIMEOUT).await {
        Ok(out) => match parse_list(&out, now) {
            Some(files) => json!({"dir": t.dir, "host": t.conn.name, "ttl_secs": TTL_SECS, "files": files}),
            None => json!({"files": [], "ttl_secs": TTL_SECS, "reason": "outbox_untrusted"}),
        },
        Err(e) => {
            tracing::warn!(host = %t.conn.name, error = %e, "could not list a remote outbox");
            json!({"files": [], "ttl_secs": TTL_SECS, "reason": "outbox_remote_unreachable", "host": t.conn.name})
        }
    };
    Ok((StatusCode::OK, axum::Json(body)).into_response())
}

/// 下載只收 outbox 最上層的一個檔名：沒有 `/`、不是隱藏檔、不在擋掉的名單裡。絕對路徑只收落在 outbox 裡那一層的。
pub fn safe_name<'a>(dir: &str, requested: &'a str) -> Option<&'a str> {
    let r = requested.trim();
    let name = r.strip_prefix(dir).and_then(|rest| rest.strip_prefix('/')).unwrap_or(r);
    if name.is_empty() || name.contains('/') || name.starts_with('.') || name.contains(['\0', '\n', '\r']) || withheld_name(&name.to_ascii_lowercase()) {
        return None;
    }
    Some(name)
}

pub fn file_script(dir: &str, name: &str) -> String {
    file_script_gap(dir, name, "")
}

/// 下載腳本。**先開 fd、再對同一個 fd 驗、只從這個 fd 讀**（#768）：遠端 bot 對自己的 outbox 有寫入權，
/// 「`-L` 檢查 → `base64 < "$F"`」是兩次路徑操作，中間能把檔案（或整個目錄）換成指到 `~/.codex/auth.json` 的符號連結。
/// 現在 `{{ … }} 3< "$F"` 先把檔案開在 fd 3（開的當下跟著連結走也沒關係），之後再驗：`$D`、`$F` 此刻都不是符號連結、`$F` 是一般檔案，
/// 而且 `$F` 跟 fd 3 是同一個檔（GNU：`-ef` 比 dev＋inode；BSD/macOS 的 `/dev/fd/N` 經 devfs 顯示 device 不同，改由 `lsof` 比較 fd 3 與重開路徑的 fd 4 之 device＋inode；無法確認就拒絕）。開檔那一刻 `$F` 若是連結，
/// fd 指到的是連結目標，之後不管 `$F` 被換成什麼，只要 device 或 inode 對不上就拒絕；開完才換成連結則 `-L` 擋下。內容只從 fd 讀，`head -c` 封頂
/// （超過上限由呼叫端判 `file_too_large`），不再事先 `wc -c "$F"`。開檔失敗（不存在、沒權限）也是 MISSING。
///
/// `gap` 是測試用的插入點（正式永遠是空字串）：在開檔之後、驗證之前跑一段，模擬 bot 在那一瞬間換檔。
pub fn file_script_gap(dir: &str, name: &str, gap: &str) -> String {
    file_script_gap_with_lsof(dir, name, gap, "/usr/sbin/lsof")
}

pub fn file_script_gap_with_lsof(dir: &str, name: &str, gap: &str, lsof_path: &str) -> String {
    file_script_race_with_lsof(dir, name, gap, "", lsof_path)
}

pub fn file_script_race_with_lsof(dir: &str, name: &str, gap: &str, after_path_checks: &str, lsof_path: &str) -> String {
    format!(
        r#"D={d}
{dir_helpers}
if ! am_enter_dir; then printf 'AM_OUTBOX_MISSING\n'; exit 0; fi
F=./{n}
if stat -c %Y . >/dev/null 2>&1; then G=1; else G=; fi
{helpers}
{{
{gap}
if ! am_dir_same || [ -L "$F" ] || [ ! -f "$F" ]; then printf 'AM_OUTBOX_MISSING\n'; exit 0; fi
{after_path_checks}
if ! am_same || ! am_singlelink; then printf 'AM_OUTBOX_MISSING\n'; exit 0; fi
printf 'AM_OUTBOX_FILE\n'
head -c {cap} <&3 | base64
}} 2>/dev/null 3< "$F" || printf 'AM_OUTBOX_MISSING\n'
"#,
        d = sh_quote(dir),
        n = sh_quote(name),
        cap = MAX_BYTES + 1,
        gap = gap,
        after_path_checks = after_path_checks,
        helpers = file_identity_helpers(lsof_path),
        dir_helpers = directory_identity_helpers(),
    )
}

pub enum Fetched {
    Missing,
    TooLarge(u64),
    File(Vec<u8>),
}

pub fn parse_file(out: &str) -> Option<Fetched> {
    let (head, rest) = out.split_once('\n').unwrap_or((out, ""));
    if head == "AM_OUTBOX_MISSING" {
        return Some(Fetched::Missing);
    }
    if let Some(n) = head.strip_prefix("AM_OUTBOX_TOO_LARGE ") {
        return Some(Fetched::TooLarge(n.trim().parse().unwrap_or(0)));
    }
    if head != "AM_OUTBOX_FILE" {
        return None;
    }
    let b64: String = rest.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(b64).ok().map(Fetched::File)
}

pub async fn file(t: Target, requested: &str) -> Result<Response, LcError> {
    if !t.conn.is_connected() {
        return Err(unreachable(&t.conn.name));
    }
    let not_found = || LcError::NotFound("file".into());
    let name = safe_name(&t.dir, requested).ok_or_else(not_found)?;
    let out = t
        .conn
        .ssh_exec_timeout(&file_script(&t.dir, name), FILE_TIMEOUT)
        .await
        .map_err(|_| LcError::conflict("outbox_remote_unreachable", json!({"reason": "outbox_remote_unreachable", "host": t.conn.name})))?;
    let data = match parse_file(&out) {
        Some(Fetched::File(data)) => data,
        Some(Fetched::TooLarge(size)) => {
            return Err(LcError::conflict("file_too_large", json!({"reason": "file_too_large", "size": size, "max": MAX_BYTES})))
        }
        Some(Fetched::Missing) | None => return Err(not_found()),
    };
    if data.len() as u64 > MAX_BYTES {
        return Err(LcError::conflict("file_too_large", json!({"reason": "file_too_large", "size": data.len(), "max": MAX_BYTES})));
    }
    if content_is_withheld(&data) {
        return Err(not_found());
    }
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime_of(std::path::Path::new(name)).to_string()),
            (header::CONTENT_DISPOSITION, content_disposition(name)),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        data,
    )
        .into_response())
}
