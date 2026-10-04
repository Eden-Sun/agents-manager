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
use crate::lifecycle::LcError;
use crate::outbox::{content_is_withheld, content_disposition, mime_of, withheld_name, MAX_BYTES, MAX_ENTRIES, TTL_SECS};
use crate::state::App;

/// 下載大檔走 base64 會比較久；列表很快。
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const FILE_TIMEOUT: Duration = Duration::from_secs(180);

/// 遠端那台上這顆 bot 的 outbox（絕對路徑）。bot id 只收英數（ULID），跟本機 [`crate::outbox::dir_for`] 同一條。
pub(crate) fn remote_dir(home: &str, instance: Option<&str>, bot_id: &str) -> Option<String> {
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
    Some(format!("{prefix}/{}/outbox/{bot_id}", crate::startup::remote_root_for(instance)))
}

pub(crate) struct Target {
    conn: Arc<HostConn>,
    dir: String,
}

fn unreachable_body(host: &str) -> serde_json::Value {
    json!({"reason": "outbox_remote_unreachable", "host": host})
}

fn unreachable(host: &str) -> LcError {
    LcError::conflict("outbox_remote_unreachable", unreachable_body(host))
}

/// 這顆 bot 在遠端主機上就回它的 outbox；本機 bot 回 `None`（照舊走 [`crate::outbox`]）。
pub(crate) async fn target(app: &Arc<App>, bot_id: &str) -> Result<Option<Target>, LcError> {
    let bot = crate::db::bot(&app.db, bot_id).await.ok().flatten().ok_or_else(|| LcError::NotFound("bot".into()))?;
    let project = crate::db::project(&app.db, &bot.project_id).await.ok().flatten().ok_or_else(|| LcError::NotFound("bot".into()))?;
    if project.host == crate::config::LOCAL_HOST {
        return Ok(None);
    }
    let conn = app.hosts.get(&project.host).await.ok_or_else(|| unreachable(&project.host))?;
    // 已知連不上（睡著、tailscale 斷線）就直接說連不上：不去等一趟 ssh（列表 30 秒、下載 180 秒才逾時），
    // 網頁每開一次也不會多養一條卡住的 ssh 行程。
    if !conn.is_connected() {
        return Err(unreachable(&project.host));
    }
    let home = conn.home().await.map_err(|_| unreachable(&project.host))?;
    let dir = remote_dir(&home, app.instance().as_deref(), &bot.id).ok_or_else(|| LcError::NotFound("bot".into()))?;
    Ok(Some(Target { conn, dir }))
}

/// 兩條遠端路徑共用的 identity 與 link-count 檢查。除 inode 外也確認 fd 解析出的實際檔名仍是 outbox 內指定項目，
/// 避免在 `-L` 檢查後換回 symlink 時只靠路徑與 fd 比對而讀到外部檔。GNU Linux 讀 `/proc/self/fd`；BSD/macOS 用 lsof。
fn file_identity_helpers(lsof_path: &str) -> String {
    file_identity_helpers_with_paths(lsof_path, "/usr/bin/lsof")
}

fn file_identity_helpers_with_paths(lsof_path: &str, fallback_lsof_path: &str) -> String {
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
fn directory_identity_helpers() -> &'static str {
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

fn list_script(dir: &str) -> String {
    list_script_race(dir, "", "")
}

fn list_script_race(dir: &str, before_open: &str, after_open: &str) -> String {
    list_script_race_with_lsof(dir, before_open, after_open, "/usr/sbin/lsof")
}

fn list_script_race_with_lsof(dir: &str, before_open: &str, after_open: &str, lsof_path: &str) -> String {
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
fn parse_list(out: &str, now: u64) -> Option<Vec<serde_json::Value>> {
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

pub(crate) async fn list(t: Target, now: u64) -> Result<Response, LcError> {
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
fn safe_name<'a>(dir: &str, requested: &'a str) -> Option<&'a str> {
    let r = requested.trim();
    let name = r.strip_prefix(dir).and_then(|rest| rest.strip_prefix('/')).unwrap_or(r);
    if name.is_empty() || name.contains('/') || name.starts_with('.') || name.contains(['\0', '\n', '\r']) || withheld_name(&name.to_ascii_lowercase()) {
        return None;
    }
    Some(name)
}

fn file_script(dir: &str, name: &str) -> String {
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
fn file_script_gap(dir: &str, name: &str, gap: &str) -> String {
    file_script_gap_with_lsof(dir, name, gap, "/usr/sbin/lsof")
}

fn file_script_gap_with_lsof(dir: &str, name: &str, gap: &str, lsof_path: &str) -> String {
    file_script_race_with_lsof(dir, name, gap, "", lsof_path)
}

fn file_script_race_with_lsof(dir: &str, name: &str, gap: &str, after_path_checks: &str, lsof_path: &str) -> String {
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

enum Fetched {
    Missing,
    TooLarge(u64),
    File(Vec<u8>),
}

fn parse_file(out: &str) -> Option<Fetched> {
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

pub(crate) async fn file(t: Target, requested: &str) -> Result<Response, LcError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 從 HTTP handler 一路走到 ssh：遠端 bot 的清單與下載都是那台上的 outbox，二進位內容原樣回來。
    #[tokio::test]
    async fn a_remote_bots_outbox_is_listed_and_downloaded_over_ssh() {
        use axum::extract::{Path as UrlPath, Query, State};
        let host = "outbox-box";
        let env = crate::testing::env().await;
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some("/Users/x".into());
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(host).bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "far").await;
        let dir = format!("/Users/x/.config/agents-manager/outbox/{}", bot.id);
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0, 0xff, 0x10];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let want_dir = format!("D={}", sh_quote(&dir));
        crate::hosts::set_ssh_fake(host, move |script| {
            assert!(script.contains(&want_dir), "{script}");
            Ok(if script.contains("| base64") {
                format!("AM_OUTBOX_FILE\n{b64}\n")
            } else {
                "AM_OUTBOX_OK\n7 2000\t89504e4700ff10\tshot.png\nAM_OUTBOX_DONE\n".into()
            })
        });
        let resp = crate::outbox::list(State(env.app.clone()), UrlPath(bot.id.clone())).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["host"], json!(host));
        assert_eq!(v["files"][0]["name"], json!("shot.png"));
        let q = Query([("path".to_string(), "shot.png".to_string())].into_iter().collect());
        let resp = crate::outbox::file(State(env.app.clone()), UrlPath(bot.id.clone()), q).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec(), png);
        let q = Query([("path".to_string(), "../etc/passwd".to_string())].into_iter().collect());
        assert!(matches!(crate::outbox::file(State(env.app.clone()), UrlPath(bot.id.clone()), q).await, Err(LcError::NotFound(_))));
    }

    /// 主機睡著／tailscale 斷線：這台已知連不上時，網頁開「檔案暫存」不能再去等一趟 ssh（列表 30 秒、下載 180 秒才逾時），
    /// 也不能每次開都多養一條卡住的 ssh 行程；直接回「連不上那台」，一個 ssh 都不打。
    #[tokio::test]
    async fn a_known_down_host_answers_unreachable_without_dialing_ssh() {
        use axum::extract::{Path as UrlPath, Query, State};
        let host = "outbox-asleep";
        let env = crate::testing::env().await;
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some("/Users/x".into()); // 連過一次，home 有快取
        assert!(!conn.is_connected());
        sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(host).bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "asleep").await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        crate::hosts::set_ssh_fake(host, move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("AM_OUTBOX_OK\n".into())
        });
        crate::hosts::set_ssh_delay(host, Duration::from_secs(3));

        let started = std::time::Instant::now();
        let resp = crate::outbox::list(State(env.app.clone()), UrlPath(bot.id.clone())).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["reason"], json!("outbox_remote_unreachable"), "{v}");
        assert_eq!(v["host"], json!(host));
        let q = Query([("path".to_string(), "shot.png".to_string())].into_iter().collect());
        match crate::outbox::file(State(env.app.clone()), UrlPath(bot.id.clone()), q).await {
            Err(LcError::Conflict(d)) => assert_eq!(d["reason"], json!("outbox_remote_unreachable")),
            other => panic!("下載要回 409 outbox_remote_unreachable：{:?}", other.map(|r| r.status())),
        }
        assert!(started.elapsed() < Duration::from_secs(1), "連不上的主機不能讓請求等：{:?}", started.elapsed());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "不該打任何 ssh");
    }

    /// The target can be selected while connected and go down before the SSH command starts.
    /// Recheck the state at the actual list/download boundary instead of dialing a host already
    /// known to be offline.
    #[tokio::test]
    async fn a_target_that_went_down_before_io_never_starts_ssh() {
        let host = "outbox-raced-down";
        let env = crate::testing::env().await;
        let cfg = crate::config::HostCfg {
            name: host.into(),
            ssh: host.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
            shared_session: false,
        };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        crate::hosts::set_ssh_fake(host, move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("AM_OUTBOX_OK\n".into())
        });

        // `target()` 已選好舊連線；監督器接著回報斷線，I/O handler 再收到這份 target。
        conn.connected.store(false, std::sync::atomic::Ordering::SeqCst);
        let list = list(Target { conn: conn.clone(), dir: "/remote/outbox/bot".into() }, 0).await.unwrap();
        let body = axum::body::to_bytes(list.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["reason"], json!("outbox_remote_unreachable"), "{body}");

        match file(Target { conn, dir: "/remote/outbox/bot".into() }, "report.txt").await {
            Err(LcError::Conflict(detail)) => assert_eq!(detail["reason"], json!("outbox_remote_unreachable"), "{detail}"),
            other => panic!("已知離線的下載應回 409：{other:?}"),
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "host became known-down before I/O; no ssh is allowed");
    }

    #[test]
    fn the_remote_dir_sits_under_the_instance_root_and_only_takes_ulids() {
        assert_eq!(remote_dir("/Users/m", None, "01ABC").as_deref(), Some("/Users/m/.config/agents-manager/outbox/01ABC"));
        assert_eq!(remote_dir("/Users/m/", None, "01ABC").as_deref(), Some("/Users/m/.config/agents-manager/outbox/01ABC"));
        assert_eq!(remote_dir("relative-home", None, "01ABC"), None, "remote home must be absolute");
        assert_eq!(remote_dir("/Users/m\nlink", None, "01ABC"), None, "control characters cannot be part of a remote home path");
        assert_eq!(remote_dir("/Users/m", Some("iso"), "01ABC").as_deref(), Some("/Users/m/.config/agents-manager/instances/iso/outbox/01ABC"));
        assert_eq!(remote_dir("/Users/m", None, "../x"), None);
        assert_eq!(remote_dir("", None, "01ABC"), None);
    }

    #[test]
    fn the_listing_drops_secrets_and_sorts_newest_first() {
        let pem = "-----BEGIN PRIVATE KEY-----".bytes().map(|b| format!("{b:02x}")).collect::<String>();
        let sqlite = "SQLite format 3\0".bytes().map(|b| format!("{b:02x}")).collect::<String>();
        let out = format!(
            "AM_OUTBOX_OK\n10 1000\t68656c6c6f\treport.md\n20 2000\t00\tshot.png\n5 3000\t{pem}\tlooks-innocent.txt\n5 3000\t{sqlite}\tdata.bin\n7 3000\t00\tid_rsa\n9 3000\t00\tprod.sqlite3\nbroken line\nAM_OUTBOX_DONE\n"
        );
        let files = parse_list(&out, 2500).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["shot.png", "report.md"]);
        assert_eq!(files[0]["remaining_secs"], json!(2000 + TTL_SECS - 2500));
        assert_eq!(files[1]["expires_at"], json!(1000 + TTL_SECS));
        assert!(parse_list("AM_OUTBOX_UNTRUSTED\n", 0).is_none());
        assert!(parse_list("", 0).is_none(), "沒有確認字就不當成成功");
        assert_eq!(parse_list("AM_OUTBOX_OK\nAM_OUTBOX_DONE\n", 0).unwrap().len(), 0);
        assert!(parse_list("AM_OUTBOX_OK\n", 0).is_none(), "missing completion marker means a partial listing");
    }

    #[test]
    fn remote_list_script_does_not_claim_success_for_an_unreadable_outbox() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = sandbox("unreadable-list");
        let dir = base.join("outbox");
        std::fs::write(dir.join("report.txt"), b"report").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let stdout = run_list(&dir);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            parse_list(&stdout, 1_000).is_none(),
            "an unreadable outbox must not be reported as a verified empty list: {stdout:?}"
        );
        let missing = base.join("not-created");
        assert_eq!(parse_list(&run_list_raw(&missing), 1_000).unwrap().len(), 0, "a verifiably missing path remains an empty list");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn remote_list_does_not_report_an_unsearchable_ancestor_as_an_empty_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = sandbox("unsearchable-parent");
        let locked = base.join("locked");
        let dir = locked.join("outbox");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.txt"), b"report").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let stdout = run_list_raw(&dir);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            parse_list(&stdout, 1_000).is_none(),
            "an inaccessible ancestor is untrusted, not a verified missing directory: {stdout:?}"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn remote_list_parser_rejects_forged_path_records() {
        let forged = "AM_OUTBOX_OK\n10 1000 1000\t00\tprivate/report.md\nAM_OUTBOX_DONE\n";
        let files = parse_list(forged, 1_000).unwrap();
        assert!(files.is_empty(), "remote records can only name one outbox entry: {files:?}");
    }

    #[test]
    fn remote_list_script_cannot_turn_a_control_filename_into_extra_records() {
        let base = sandbox("control-name");
        let dir = base.join("outbox");
        let forged_name = "noise.txt\n10 1000 1000\t00\tspoofed.txt";
        std::fs::write(dir.join(forged_name), b"x").unwrap();

        let stdout = run_list(&dir);
        let files = parse_list(&stdout, 1_000).unwrap();
        assert!(files.iter().all(|f| f["name"] != "spoofed.txt"), "control bytes must not forge records: {stdout:?}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 第三欄是 ctime（搬進來的時間）：到期從 mtime 與 ctime 較晚的那個起算，`modified` 還是 mtime。
    #[test]
    fn a_remote_file_expires_from_the_later_of_mtime_and_ctime() {
        let out = "AM_OUTBOX_OK\n10 1000 3000\t00\tmoved-in.pdf\n10 2000 1500\t00\tclock-skew.txt\nAM_OUTBOX_DONE\n";
        let files = parse_list(out, 3100).unwrap();
        let by = |n: &str| files.iter().find(|f| f["name"] == n).unwrap().clone();
        assert_eq!(by("moved-in.pdf")["modified"], json!(1000));
        assert_eq!(by("moved-in.pdf")["expires_at"], json!(3000 + TTL_SECS));
        assert_eq!(by("clock-skew.txt")["expires_at"], json!(2000 + TTL_SECS), "mtime 比 ctime 晚（時鐘不準）就看 mtime");
    }

    #[test]
    fn downloads_take_one_plain_name_inside_the_outbox() {
        let d = "/Users/m/.config/agents-manager/outbox/01ABC";
        assert_eq!(safe_name(d, "report.md"), Some("report.md"));
        assert_eq!(safe_name(d, &format!("{d}/report.md")), Some("report.md"));
        for bad in ["../x", "sub/x", ".env", "", "/etc/passwd", "key.pem", "a\nb", "prod.sqlite3"] {
            assert_eq!(safe_name(d, bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_scripts_quote_their_paths_and_say_when_they_are_done() {
        let s = list_script("/U/m x/outbox/01A");
        assert!(s.contains("D='/U/m x/outbox/01A'") && s.contains("-mmin +60") && s.contains("-cmin +60") && s.contains("printf '%s\\t%s\\t%s\\n'"), "{s}");
        let f = file_script("/U/o", "it's.md");
        assert!(f.contains(r#"F=./'it'\''s.md'"#) && f.contains("am_enter_dir") && f.contains(&format!("head -c {}", MAX_BYTES + 1)) && f.contains("3< \"$F\""), "{f}");
    }

    #[test]
    fn a_fetched_file_is_decoded_byte_for_byte() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        // GNU base64 每 76 字換行，macOS 不換：兩種都要解得回來。
        let wrapped: String = b64.as_bytes().chunks(76).map(|c| std::str::from_utf8(c).unwrap().to_string() + "\n").collect();
        for body in [b64.clone() + "\n", wrapped] {
            match parse_file(&format!("AM_OUTBOX_FILE\n{body}")) {
                Some(Fetched::File(d)) => assert_eq!(d, bytes),
                _ => panic!("decode failed"),
            }
        }
        assert!(matches!(parse_file("AM_OUTBOX_MISSING\n"), Some(Fetched::Missing)));
        assert!(matches!(parse_file("AM_OUTBOX_TOO_LARGE 99\n"), Some(Fetched::TooLarge(99))));
        assert!(parse_file("garbage").is_none());
    }

    /// 在本機用 `sh` 跑遠端的下載腳本（`gap` 在檢查與讀檔之間換檔，模擬遠端 bot 的競態），回腳本輸出。
    fn run_download_raw(dir: &std::path::Path, name: &str, gap: &str) -> String {
        let script = file_script_gap(&dir.to_string_lossy(), name, gap);
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn run_download(dir: &std::path::Path, name: &str, gap: &str) -> String {
        let physical = std::fs::canonicalize(dir).unwrap();
        run_download_raw(&physical, name, gap)
    }

    fn run_list_raw(dir: &std::path::Path) -> String {
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(list_script(&dir.to_string_lossy()))
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn run_list(dir: &std::path::Path) -> String {
        let physical = std::fs::canonicalize(dir).unwrap();
        run_list_raw(&physical)
    }

    /// Simulate BSD `stat`: `stat -c` is rejected and `/dev/fd/3` reports metadata on the opened
    /// descriptor. The fake can give the pathname and descriptor different devices while keeping
    /// or changing the inode, matching macOS devfs behavior without requiring a second filesystem.
    fn run_download_with_bsd_stat(
        dir: &std::path::Path,
        name: &str,
        gap: &str,
        path_inode: u64,
        fd_inode: u64,
    ) -> (String, std::path::PathBuf) {
        run_download_with_bsd_stat_devices(dir, name, gap, 1, path_inode, 1, fd_inode, 1)
    }

    fn bsd_stat_tools(
        path_device: u64,
        path_inode: u64,
        fd_device: u64,
        fd_inode: u64,
        nlink: u64,
    ) -> (std::path::PathBuf, String) {
        let bin = crate::testing::track(std::env::temp_dir().join(format!("am-outbox-bsd-stat-{}", crate::db::ulid())));
        std::fs::create_dir_all(&bin).unwrap();
        let stat = bin.join("stat");
        std::fs::write(
            &stat,
            format!(
                r#"#!/bin/sh
if [ "$1" = "-c" ]; then exit 1; fi
if [ "$1" = "-L" ] && [ "$2" = "-f" ]; then
  case "$3" in
    '%i %z %m %c')
      case "$4" in /dev/fd/3) printf '{fd_inode} 5 100 100\n' ;; *) printf '{path_inode} 5 100 100\n' ;; esac
      ;;
    '%d %i %z %m %c')
      case "$4" in /dev/fd/3) printf '{fd_device} {fd_inode} 5 100 100\n' ;; *) printf '{path_device} {path_inode} 5 100 100\n' ;; esac
      ;;
    '%z %m %c') echo '5 100 100' ;;
    '%l') printf '%s\n' '%l' >> "$AM_TEST_STAT_LOG"; echo {nlink} ;;
    *) exit 1 ;;
  esac
  exit 0
fi
exec /usr/bin/stat "$@"
"#,
                path_inode = path_inode,
                fd_inode = fd_inode,
                path_device = path_device,
                fd_device = fd_device,
                nlink = nlink,
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stat, std::fs::Permissions::from_mode(0o755)).unwrap();

        let lsof = bin.join("lsof");
        std::fs::write(
            &lsof,
            format!(
                r#"#!/bin/sh
pid=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-p" ]; then pid=$2; shift 2; else shift; fi
done
if [ -d /proc ] && [ ! -e "/proc/$pid/fd/3" ]; then exit 1; fi
printf 'p%s\nf3r\nD0x{fd_device:x}\ni{fd_inode}\nn%s\nf4r\nD0x{path_device:x}\ni{path_inode}\nn%s\n' "$pid" "$AM_TEST_FD3_NAME" "$AM_TEST_FD4_NAME"
"#,
                path_device = path_device,
                path_inode = path_inode,
                fd_device = fd_device,
                fd_inode = fd_inode,
            ),
        )
        .unwrap();
        std::fs::set_permissions(&lsof, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        (bin, path)
    }

    fn install_lsof(bin: &std::path::Path, script: &str) {
        let lsof = bin.join("lsof");
        std::fs::write(&lsof, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&lsof, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run_download_with_bsd_stat_devices(
        dir: &std::path::Path,
        name: &str,
        gap: &str,
        path_device: u64,
        path_inode: u64,
        fd_device: u64,
        fd_inode: u64,
        nlink: u64,
    ) -> (String, std::path::PathBuf) {
        let (bin, path) = bsd_stat_tools(path_device, path_inode, fd_device, fd_inode, nlink);
        let lsof_path = bin.join("lsof");
        let physical = std::fs::canonicalize(dir).unwrap();
        let script = file_script_gap_with_lsof(&physical.to_string_lossy(), name, gap, &lsof_path.to_string_lossy());
        let expected_name = format!("{}/{}", physical.display(), name);
        let stat_log = bin.join("stat.log");
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("PATH", path)
            .env("AM_TEST_STAT_LOG", &stat_log)
            .env("AM_TEST_FD3_NAME", &expected_name)
            .env("AM_TEST_FD4_NAME", &expected_name)
            .output()
            .unwrap();
        (String::from_utf8_lossy(&out.stdout).into_owned(), bin)
    }

    fn run_list_with_bsd_stat_devices(
        dir: &std::path::Path,
        before_open: &str,
        after_open: &str,
        path_device: u64,
        path_inode: u64,
        fd_device: u64,
        fd_inode: u64,
        nlink: u64,
    ) -> (String, std::path::PathBuf) {
        let (bin, path) = bsd_stat_tools(path_device, path_inode, fd_device, fd_inode, nlink);
        let lsof_path = bin.join("lsof");
        let physical = std::fs::canonicalize(dir).unwrap();
        let script = list_script_race_with_lsof(&physical.to_string_lossy(), before_open, after_open, &lsof_path.to_string_lossy());
        let expected_name = format!("{}/report.txt", physical.display());
        let stat_log = bin.join("stat.log");
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("PATH", path)
            .env("AM_TEST_STAT_LOG", &stat_log)
            .env("AM_TEST_FD3_NAME", &expected_name)
            .env("AM_TEST_FD4_NAME", &expected_name)
            .output()
            .unwrap();
        (String::from_utf8_lossy(&out.stdout).into_owned(), bin)
    }

    fn run_link_count_probe(gnu: bool, nlink: u64) -> (String, std::path::PathBuf) {
        let bin = crate::testing::track(std::env::temp_dir().join(format!("am-outbox-link-probe-{}", crate::db::ulid())));
        std::fs::create_dir_all(&bin).unwrap();
        let stat = bin.join("stat");
        let stat_body = if gnu {
            format!(
                r#"#!/bin/sh
if [ "$1" = "-L" ]; then shift; fi
if [ "$1" = "-c" ] && [ "$2" = "%h" ]; then printf '%s\n' '%h' >> "$AM_TEST_STAT_LOG"; echo {nlink}; exit 0; fi
exit 1
"#
            )
        } else {
            format!(
                r#"#!/bin/sh
if [ "$1" = "-L" ] && [ "$2" = "-f" ] && [ "$3" = "%l" ]; then printf '%s\n' '%l' >> "$AM_TEST_STAT_LOG"; echo {nlink}; exit 0; fi
exit 1
"#
            )
        };
        std::fs::write(&stat, stat_body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stat, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let stat_log = bin.join("stat.log");
        let script = format!(
            "G={};\n{}\nexec 3</dev/null\nif am_singlelink; then echo single; else echo linked; fi\n",
            if gnu { "1" } else { "" },
            file_identity_helpers("/usr/sbin/lsof"),
        );
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("PATH", path)
            .env("AM_TEST_STAT_LOG", stat_log)
            .output()
            .unwrap();
        (String::from_utf8_lossy(&out.stdout).into_owned(), bin)
    }

    fn served(out: &str) -> Option<Vec<u8>> {
        match parse_file(out) {
            Some(Fetched::File(d)) => Some(d),
            _ => None,
        }
    }

    fn sandbox(tag: &str) -> std::path::PathBuf {
        let base = crate::testing::track(std::env::temp_dir().join(format!("am-outbox-remote-{tag}-{}", crate::db::ulid())));
        std::fs::create_dir_all(base.join("outbox")).unwrap();
        std::fs::write(base.join("secret.txt"), b"TOP SECRET").unwrap();
        base
    }

    /// 正常下載：一般檔案原樣回來；符號連結、目錄、不存在都是 MISSING。
    #[test]
    fn macos_local_remote_download_serves_a_regular_file_only() {
        let base = sandbox("plain");
        let d = base.join("outbox");
        std::fs::write(d.join("report.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink(base.join("secret.txt"), d.join("link.txt")).unwrap();
        std::fs::create_dir(d.join("sub")).unwrap();
        assert_eq!(served(&run_download(&d, "report.txt", "")).as_deref(), Some(&b"hello"[..]));
        for bad in ["link.txt", "sub", "missing.txt"] {
            assert_eq!(run_download(&d, bad, "").trim(), "AM_OUTBOX_MISSING", "{bad}");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn macos_local_remote_outbox_hard_links_are_neither_listed_nor_downloaded() {
        let base = sandbox("hardlink");
        let d = base.join("outbox");
        std::fs::write(base.join("secret.txt"), b"private bytes").unwrap();
        std::fs::hard_link(base.join("secret.txt"), d.join("report.txt")).unwrap();
        let listed = parse_list(&run_list(&d), 1_000).unwrap();
        assert!(!listed.iter().any(|f| f["name"] == "report.txt"), "hard link was listed: {listed:?}");
        assert_eq!(run_download(&d, "report.txt", "").trim(), "AM_OUTBOX_MISSING");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn macos_local_remote_download_rejects_a_symlink_restored_between_path_checks() {
        let base = sandbox("check-race");
        let d = base.join("outbox");
        let secret = base.join("secret.txt");
        std::fs::write(&secret, b"TOP SECRET").unwrap();
        std::os::unix::fs::symlink(&secret, d.join("report.txt")).unwrap();
        let physical = std::fs::canonicalize(&d).unwrap();
        let opened_from_outside = "rm -f \"$F\"; printf decoy > \"$F\"";
        let restored_symlink = format!("rm -f \"$F\"; ln -s {} \"$F\"", sh_quote(&secret.to_string_lossy()));
        let script = file_script_race_with_lsof(&physical.to_string_lossy(), "report.txt", opened_from_outside, &restored_symlink, "/usr/sbin/lsof");
        let result = std::process::Command::new("/bin/sh").arg("-c").arg(script).output().unwrap();
        let out = String::from_utf8_lossy(&result.stdout).into_owned();
        assert_eq!(served(&out), None, "a symlink restored after the path checks leaked its already-open target: {out}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn remote_outbox_rejects_symlinked_parent_components_for_list_and_download() {
        let base = sandbox("parent-link");
        let outside = base.join("outside");
        let outside_bot = outside.join("BOT01");
        std::fs::create_dir_all(&outside_bot).unwrap();
        std::fs::write(outside_bot.join("report.txt"), b"outside secret").unwrap();
        let linked_parent = base.join("outbox-parent-link");
        std::os::unix::fs::symlink(&outside, &linked_parent).unwrap();
        let dir = linked_parent.join("BOT01");
        assert!(!std::fs::symlink_metadata(&dir).unwrap().file_type().is_symlink(), "the final component itself is a regular directory");

        let download = run_download_raw(&dir, "report.txt", "");
        let listing = run_list_raw(&dir);
        let leaked = parse_list(&listing, 1_000).is_some_and(|files| files.iter().any(|f| f["name"] == "report.txt"));
        assert_eq!(download.trim(), "AM_OUTBOX_MISSING", "download followed an ancestor symlink: {download:?}");
        assert!(!leaked, "listing followed an ancestor symlink: {listing:?}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A listing also performs the remote TTL sweep, so a parent symlink must be rejected before
    /// `find` can remove a stale file outside the trusted outbox tree.
    #[cfg(unix)]
    #[test]
    fn macos_local_remote_listing_does_not_gc_through_a_symlinked_outbox_parent() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let base = sandbox("gc-symlink-parent");
        let home = base.join("home");
        let real_config = base.join("external-config");
        let outbox = real_config.join("agents-manager/outbox/01BOT");
        std::fs::create_dir_all(&outbox).unwrap();
        let stale = outbox.join("stale.txt");
        std::fs::write(&stale, b"keep outside").unwrap();
        std::process::Command::new("touch").args(["-t", "200001010000"]).arg(&stale).status().unwrap();
        std::fs::create_dir_all(&home).unwrap();
        symlink(&real_config, home.join(".config")).unwrap();

        let linked_outbox = home.join(".config/agents-manager/outbox/01BOT");
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let find = bin.join("find");
        std::fs::write(
            &find,
            "#!/bin/sh\nexec /usr/bin/find \"$1\" -maxdepth 1 -type f -mmin +0 -exec rm -f {} +\n",
        )
        .unwrap();
        std::fs::set_permissions(&find, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(list_script_race_with_lsof(&linked_outbox.to_string_lossy(), "", "", "/usr/sbin/lsof"))
            .env("PATH", path)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stale.exists(), "the remote listing GC must not delete through a parent symlink");
        assert!(stdout.starts_with("AM_OUTBOX_UNTRUSTED"), "symlinked parent must fail closed: {stdout}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// #768：檢查之後、讀檔之前，bot 把檔案換成指到界線外的符號連結。以前 `base64 < "$F"` 照著連結讀出來。
    #[test]
    fn macos_local_a_symlink_swapped_in_after_the_check_is_never_served() {
        let base = sandbox("swap-after");
        let d = base.join("outbox");
        std::fs::write(d.join("report.txt"), b"hello").unwrap();
        let gap = format!("rm -f \"$F\"; ln -s '{}' \"$F\"", base.join("secret.txt").display());
        let out = run_download(&d, "report.txt", &gap);
        assert!(!out.contains("TOP SECRET") && served(&out).as_deref() != Some(&b"TOP SECRET"[..]), "{out}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 反過來：開檔的那一刻 `$F` 還是指到界線外的符號連結，開完之後被換回一般檔案（檢查看到的是一般檔案）。
    /// 腳本讀的是開檔當下那個 inode，不是檢查看到的那個。
    #[test]
    fn macos_local_a_file_swapped_back_after_opening_the_link_is_never_served() {
        let base = sandbox("swap-back");
        let d = base.join("outbox");
        std::os::unix::fs::symlink(base.join("secret.txt"), d.join("report.txt")).unwrap();
        let gap = "rm -f \"$F\"; echo decoy > \"$F\"";
        let out = run_download(&d, "report.txt", gap);
        assert!(!out.contains("TOP SECRET") && served(&out).as_deref() != Some(&b"TOP SECRET"[..]), "{out}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 目錄本身被換成符號連結（指到別的目錄、裡面剛好有同名檔）也不行。
    #[test]
    fn macos_local_an_outbox_dir_swapped_for_a_symlink_is_never_served() {
        let base = sandbox("dir-link");
        let real = base.join("elsewhere");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("report.txt"), b"TOP SECRET").unwrap();
        let d = base.join("outbox-link");
        std::os::unix::fs::symlink(&real, &d).unwrap();
        let out = run_download_raw(&d, "report.txt", "");
        assert_eq!(out.trim(), "AM_OUTBOX_MISSING", "{out}");
        let d2 = base.join("outbox");
        std::fs::write(d2.join("report.txt"), b"hello").unwrap();
        let gap = format!("mv \"$D\" \"$D.real\"; ln -s '{}' \"$D\"", real.display());
        let out = run_download(&d2, "report.txt", &gap);
        assert!(!out.contains("TOP SECRET") && served(&out).as_deref() != Some(&b"TOP SECRET"[..]), "{out}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// macOS `stat` on `/dev/fd/3` reports the devfs device, so identity uses `lsof` fstat of fd 3 and a
    /// reopened fd 4. Matching device and inode still serves the file.
    #[test]
    fn macos_local_bsd_stat_accepts_the_same_inode_across_devfs_devices() {
        let base = sandbox("bsd-stat-same-inode");
        let d = base.join("outbox");
        std::fs::write(d.join("report.txt"), b"hello").unwrap();
        let (out, shim) = run_download_with_bsd_stat(&d, "report.txt", "", 42, 42);
        assert_eq!(served(&out).as_deref(), Some(&b"hello"[..]), "matching inode must survive a devfs device mismatch: {out}");
        std::fs::remove_dir_all(&shim).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A file replaced after opening has a different inode even if both stats see a different
    /// device for `/dev/fd/3`; removing the device comparison must still reject the stale fd.
    #[test]
    fn macos_local_bsd_stat_rejects_a_replaced_inode_across_devfs_devices() {
        let base = sandbox("bsd-stat-replaced-inode");
        let d = base.join("outbox");
        std::os::unix::fs::symlink(base.join("secret.txt"), d.join("report.txt")).unwrap();
        let gap = "rm -f \"$F\"; printf decoy > \"$F\"";
        let (out, shim) = run_download_with_bsd_stat(&d, "report.txt", gap, 43, 42);
        assert!(!out.contains("TOP SECRET") && served(&out).as_deref() != Some(&b"TOP SECRET"[..]), "secret descriptor was accepted: {out}");
        std::fs::remove_dir_all(&shim).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 同一 inode 號與 metadata 不代表同一檔案：不同 filesystem 可重用 inode，不能因此把已開啟的外部檔案當成 outbox 項目。
    #[test]
    fn macos_local_bsd_stat_download_rejects_a_cross_device_inode_collision() {
        let base = sandbox("bsd-cross-device-download");
        let d = base.join("outbox");
        std::os::unix::fs::symlink(base.join("secret.txt"), d.join("report.txt")).unwrap();
        let gap = "rm -f \"$F\"; printf decoy > \"$F\"";
        let (out, shim) = run_download_with_bsd_stat_devices(&d, "report.txt", gap, 2, 42, 1, 42, 1);
        assert_eq!(out.trim(), "AM_OUTBOX_MISSING", "same inode/size/times on different devices must not pass: {out}");
        std::fs::remove_dir_all(&shim).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn macos_local_bsd_stat_listing_rejects_a_cross_device_inode_collision() {
        let base = sandbox("bsd-cross-device-list");
        let d = base.join("outbox");
        std::fs::write(d.join("report.txt"), b"decoy").unwrap();
        let before_open = format!("mv \"$F\" \"$F.saved\"; ln -s '{}' \"$F\"", base.join("secret.txt").display());
        let after_open = "rm -f \"$F\"; mv \"$F.saved\" \"$F\"";
        let (out, shim) = run_list_with_bsd_stat_devices(&d, &before_open, after_open, 2, 42, 1, 42, 1);
        let listed = parse_list(&out, 1_000).unwrap();
        assert!(!listed.iter().any(|f| f["name"] == "report.txt"), "listing must reject a cross-device identity collision: {listed:?}");
        std::fs::remove_dir_all(&shim).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn macos_local_both_gnu_h_and_bsd_l_link_count_probes_reject_hard_links() {
        for (gnu, expected_probe) in [(true, "%h"), (false, "%l")] {
            let (out, temp) = run_link_count_probe(gnu, 2);
            assert_eq!(out.trim(), "linked", "GNU stat={gnu}: {out}");
            assert_eq!(std::fs::read_to_string(temp.join("stat.log")).unwrap().trim(), expected_probe);
            std::fs::remove_dir_all(&temp).unwrap();
        }
    }

    /// BSD 腳本的 `%l` 必須真的接在列檔與下載上：nlink 2 跟 device 相同也不能放行。
    #[test]
    fn macos_local_bsd_stat_listing_and_download_reject_extra_hard_links() {
        let base = sandbox("bsd-nlink");
        let d = base.join("outbox");
        std::fs::write(d.join("report.txt"), b"hello").unwrap();
        let (out, shim) = run_download_with_bsd_stat_devices(&d, "report.txt", "", 1, 42, 1, 42, 2);
        assert_eq!(out.trim(), "AM_OUTBOX_MISSING", "download accepted nlink 2: {out}");
        std::fs::remove_dir_all(&shim).unwrap();
        let (listed_out, shim_list) = run_list_with_bsd_stat_devices(&d, "", "", 1, 42, 1, 42, 2);
        let listed = parse_list(&listed_out, 1_000).unwrap();
        assert!(!listed.iter().any(|f| f["name"] == "report.txt"), "listing accepted nlink 2: {listed:?}");
        std::fs::remove_dir_all(&shim_list).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 不完整／失敗的 lsof 即使先吐出看似相同的 fd identity，也不能因 awk 的成功狀態而放行。
    #[test]
    fn macos_local_bsd_stat_rejects_an_open_fd_named_outside_the_outbox() {
        let base = sandbox("bsd-fd-name");
        let d = base.join("outbox");
        std::fs::write(d.join("report.txt"), b"hello").unwrap();
        let (bin, path) = bsd_stat_tools(1, 42, 1, 42, 1);
        let lsof_path = bin.join("lsof");
        let physical = std::fs::canonicalize(&d).unwrap();
        let script = file_script_gap_with_lsof(&physical.to_string_lossy(), "report.txt", "", &lsof_path.to_string_lossy());
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("PATH", path)
            .env("AM_TEST_STAT_LOG", bin.join("stat.log"))
            .env("AM_TEST_FD3_NAME", "/outside/private-key")
            .env("AM_TEST_FD4_NAME", "/outside/private-key")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "AM_OUTBOX_MISSING", "fd path outside the outbox was accepted");
        std::fs::remove_dir_all(&bin).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn macos_local_lsof_partial_output_then_slow_failure_is_rejected_by_listing_and_download() {
        let base = sandbox("lsof-partial-failure");
        let d = base.join("outbox");
        std::fs::write(d.join("report.txt"), b"hello").unwrap();
        let lsof = "#!/bin/sh\nprintf 'f3r\\nD0x1\\ni42\\nn%s\\nf4r\\nD0x1\\ni42\\nn%s\\n' \"$AM_TEST_FD3_NAME\" \"$AM_TEST_FD4_NAME\"\nsleep 0.05\nexit 7\n";

        let (download_bin, path) = bsd_stat_tools(1, 42, 1, 42, 1);
        install_lsof(&download_bin, lsof);
        let physical = std::fs::canonicalize(&d).unwrap();
        let expected_name = format!("{}/report.txt", physical.display());
        let download_script = file_script_gap_with_lsof(&physical.to_string_lossy(), "report.txt", "", &download_bin.join("lsof").to_string_lossy());
        let download = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(download_script)
            .env("PATH", &path)
            .env("AM_TEST_STAT_LOG", download_bin.join("stat.log"))
            .env("AM_TEST_FD3_NAME", &expected_name)
            .env("AM_TEST_FD4_NAME", &expected_name)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&download.stdout).trim(), "AM_OUTBOX_MISSING", "failed lsof output must not authorize download");

        let (list_bin, path) = bsd_stat_tools(1, 42, 1, 42, 1);
        install_lsof(&list_bin, lsof);
        let list_script = list_script_race_with_lsof(&physical.to_string_lossy(), "", "", &list_bin.join("lsof").to_string_lossy());
        let listed = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(list_script)
            .env("PATH", path)
            .env("AM_TEST_STAT_LOG", list_bin.join("stat.log"))
            .env("AM_TEST_FD3_NAME", &expected_name)
            .env("AM_TEST_FD4_NAME", &expected_name)
            .output()
            .unwrap();
        let files = parse_list(&String::from_utf8_lossy(&listed.stdout), 1_000).unwrap();
        assert!(!files.iter().any(|f| f["name"] == "report.txt"), "failed lsof output must not authorize listing: {files:?}");

        std::fs::remove_dir_all(&download_bin).unwrap();
        std::fs::remove_dir_all(&list_bin).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn macos_local_lsof_unknown_device_format_is_rejected_by_listing_and_download() {
        let base = sandbox("lsof-unknown-device");
        let d = base.join("outbox");
        std::fs::write(d.join("report.txt"), b"hello").unwrap();
        let lsof = "#!/bin/sh\nprintf 'f3r\\nD?\\ni42\\nn%s\\nf4r\\nD?\\ni42\\nn%s\\n' \"$AM_TEST_FD3_NAME\" \"$AM_TEST_FD4_NAME\"\n";

        let (download_bin, path) = bsd_stat_tools(1, 42, 1, 42, 1);
        install_lsof(&download_bin, lsof);
        let physical = std::fs::canonicalize(&d).unwrap();
        let expected_name = format!("{}/report.txt", physical.display());
        let download_script = file_script_gap_with_lsof(&physical.to_string_lossy(), "report.txt", "", &download_bin.join("lsof").to_string_lossy());
        let download = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(download_script)
            .env("PATH", &path)
            .env("AM_TEST_STAT_LOG", download_bin.join("stat.log"))
            .env("AM_TEST_FD3_NAME", &expected_name)
            .env("AM_TEST_FD4_NAME", &expected_name)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&download.stdout).trim(), "AM_OUTBOX_MISSING", "unknown device IDs cannot prove identity");

        let (list_bin, path) = bsd_stat_tools(1, 42, 1, 42, 1);
        install_lsof(&list_bin, lsof);
        let list_script = list_script_race_with_lsof(&physical.to_string_lossy(), "", "", &list_bin.join("lsof").to_string_lossy());
        let listed = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(list_script)
            .env("PATH", path)
            .env("AM_TEST_STAT_LOG", list_bin.join("stat.log"))
            .env("AM_TEST_FD3_NAME", &expected_name)
            .env("AM_TEST_FD4_NAME", &expected_name)
            .output()
            .unwrap();
        let files = parse_list(&String::from_utf8_lossy(&listed.stdout), 1_000).unwrap();
        assert!(!files.iter().any(|f| f["name"] == "report.txt"), "unknown device IDs cannot prove identity: {files:?}");

        std::fs::remove_dir_all(&download_bin).unwrap();
        std::fs::remove_dir_all(&list_bin).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn macos_local_missing_lsof_fails_closed() {
        let base = sandbox("lsof-missing");
        let d = base.join("outbox");
        std::fs::write(d.join("report.txt"), b"hello").unwrap();
        let physical = std::fs::canonicalize(&d).unwrap();
        let helpers = file_identity_helpers_with_paths("/missing/primary-lsof", "/missing/fallback-lsof");
        let script = format!(
            "D={};\n{}\nam_enter_dir || exit 1\nF=./report.txt; G=;\n{}\nexec 3< \"$F\"\nif am_same; then echo same; else echo unverified; fi\n",
            sh_quote(&physical.to_string_lossy()),
            directory_identity_helpers(),
            helpers,
        );
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "unverified");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
