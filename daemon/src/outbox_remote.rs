//! 遠端主機 bot 的 outbox（SPEC §6.5f，使用者 2026-10-01「想辦法解決」）。
//!
//! 以前遠端 bot 沒有 `AM_OUTBOX`，網頁只說「檔案不在這台機器，列不出來」。現在遠端 pane 也拿到 `AM_OUTBOX`，
//! 指到**那台主機上**的 `~/<remote root>/outbox/<bot_id>/`（跟 bot 目錄同一個實例根），網頁列表與下載走 ssh：
//!
//! - 只列 outbox 最上層的一般檔（`find -type f` 的語意：符號連結不算），outbox 本身是符號連結就整個不列。
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
    if bot_id.is_empty() || !bot_id.chars().all(|c| c.is_ascii_alphanumeric()) || home.trim().is_empty() {
        return None;
    }
    Some(format!("{home}/{}/outbox/{bot_id}", crate::startup::remote_root_for(instance)))
}

pub(crate) struct Target {
    conn: Arc<HostConn>,
    dir: String,
}

/// 這顆 bot 在遠端主機上就回它的 outbox；本機 bot 回 `None`（照舊走 [`crate::outbox`]）。
pub(crate) async fn target(app: &Arc<App>, bot_id: &str) -> Result<Option<Target>, LcError> {
    let bot = crate::db::bot(&app.db, bot_id).await.ok().flatten().ok_or_else(|| LcError::NotFound("bot".into()))?;
    let project = crate::db::project(&app.db, &bot.project_id).await.ok().flatten().ok_or_else(|| LcError::NotFound("bot".into()))?;
    if project.host == crate::config::LOCAL_HOST {
        return Ok(None);
    }
    let unreachable = || LcError::conflict("outbox_remote_unreachable", json!({"reason": "outbox_remote_unreachable", "host": project.host}));
    let conn = app.hosts.get(&project.host).await.ok_or_else(unreachable)?;
    let home = conn.home().await.map_err(|_| unreachable())?;
    let dir = remote_dir(&home, app.instance().as_deref(), &bot.id).ok_or_else(|| LcError::NotFound("bot".into()))?;
    Ok(Some(Target { conn, dir }))
}

fn list_script(dir: &str) -> String {
    format!(
        r#"D={d}
if [ -L "$D" ]; then printf 'AM_OUTBOX_UNTRUSTED\n'; exit 0; fi
if [ ! -d "$D" ]; then printf 'AM_OUTBOX_OK\n'; exit 0; fi
find "$D" -maxdepth 1 -type f -mmin +{ttl_min} -cmin +{ttl_min} -exec rm -f {{}} + 2>/dev/null
if stat -c %Y "$D" >/dev/null 2>&1; then G=1; else G=; fi
printf 'AM_OUTBOX_OK\n'
for f in "$D"/*; do
  [ -f "$f" ] && [ ! -L "$f" ] || continue
  if [ -n "$G" ]; then s=$(stat -c '%s %Y %Z' "$f"); else s=$(stat -f '%z %m %c' "$f"); fi
  h=$(od -An -tx1 -N64 "$f" | tr -d ' \n')
  printf '%s\t%s\t%s\n' "$s" "$h" "${{f##*/}}"
done
"#,
        d = sh_quote(dir),
        ttl_min = TTL_SECS / 60,
    )
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()).collect()
}

/// `list_script` 的輸出 → 跟本機 `outbox::scan` 同形狀的清單。`None`＝outbox 是符號連結（不可信）。
fn parse_list(out: &str, now: u64) -> Option<Vec<serde_json::Value>> {
    let mut lines = out.lines();
    match lines.next() {
        Some("AM_OUTBOX_OK") => {}
        _ => return None,
    }
    let mut files: Vec<(String, u64, u64, u64)> = Vec::new();
    for line in lines {
        let mut parts = line.splitn(3, '\t');
        let (Some(meta), Some(hex), Some(name)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let mut meta = meta.split_whitespace();
        let (Some(Ok(size)), Some(Ok(modified))) = (meta.next().map(str::parse::<u64>), meta.next().map(str::parse::<u64>)) else { continue };
        // 第三欄 ctime＝搬進來的時間；舊格式沒有這一欄就只看 mtime。
        let changed = meta.next().and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
        if name.is_empty() || withheld_name(&name.to_ascii_lowercase()) || content_is_withheld(&hex_bytes(hex)) {
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
/// 而且 `$F` 跟 fd 3 是同一個 inode（`-ef`，`/dev/fd/3`：GNU 與 BSD／macOS 的 `test`、`/dev/fd` 都有）。開檔那一刻 `$F` 若是連結，
/// fd 指到的是連結目標，之後不管 `$F` 被換成什麼，inode 對不上就拒絕；開完才換成連結則 `-L` 擋下。內容只從 fd 讀，`head -c` 封頂
/// （超過上限由呼叫端判 `file_too_large`），不再事先 `wc -c "$F"`。開檔失敗（不存在、沒權限）也是 MISSING。
///
/// `gap` 是測試用的插入點（正式永遠是空字串）：在開檔之後、驗證之前跑一段，模擬 bot 在那一瞬間換檔。
fn file_script_gap(dir: &str, name: &str, gap: &str) -> String {
    format!(
        r#"D={d}
F="$D"/{n}
{{
{gap}
if [ -L "$D" ] || [ -L "$F" ] || [ ! -f "$F" ] || [ ! "$F" -ef /dev/fd/3 ]; then printf 'AM_OUTBOX_MISSING\n'; exit 0; fi
printf 'AM_OUTBOX_FILE\n'
head -c {cap} <&3 | base64
}} 2>/dev/null 3< "$F" || printf 'AM_OUTBOX_MISSING\n'
"#,
        d = sh_quote(dir),
        n = sh_quote(name),
        cap = MAX_BYTES + 1,
        gap = gap,
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
                "AM_OUTBOX_OK\n7 2000\t89504e4700ff10\tshot.png\n".into()
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

    #[test]
    fn the_remote_dir_sits_under_the_instance_root_and_only_takes_ulids() {
        assert_eq!(remote_dir("/Users/m", None, "01ABC").as_deref(), Some("/Users/m/.config/agents-manager/outbox/01ABC"));
        assert_eq!(remote_dir("/Users/m", Some("iso"), "01ABC").as_deref(), Some("/Users/m/.config/agents-manager/instances/iso/outbox/01ABC"));
        assert_eq!(remote_dir("/Users/m", None, "../x"), None);
        assert_eq!(remote_dir("", None, "01ABC"), None);
    }

    #[test]
    fn the_listing_drops_secrets_and_sorts_newest_first() {
        let pem = "-----BEGIN PRIVATE KEY-----".bytes().map(|b| format!("{b:02x}")).collect::<String>();
        let sqlite = "SQLite format 3\0".bytes().map(|b| format!("{b:02x}")).collect::<String>();
        let out = format!(
            "AM_OUTBOX_OK\n10 1000\t68656c6c6f\treport.md\n20 2000\t00\tshot.png\n5 3000\t{pem}\tlooks-innocent.txt\n5 3000\t{sqlite}\tdata.bin\n7 3000\t00\tid_rsa\n9 3000\t00\tprod.sqlite3\nbroken line\n"
        );
        let files = parse_list(&out, 2500).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["shot.png", "report.md"]);
        assert_eq!(files[0]["remaining_secs"], json!(2000 + TTL_SECS - 2500));
        assert_eq!(files[1]["expires_at"], json!(1000 + TTL_SECS));
        assert!(parse_list("AM_OUTBOX_UNTRUSTED\n", 0).is_none());
        assert!(parse_list("", 0).is_none(), "沒有確認字就不當成成功");
        assert_eq!(parse_list("AM_OUTBOX_OK\n", 0).unwrap().len(), 0);
    }

    /// 第三欄是 ctime（搬進來的時間）：到期從 mtime 與 ctime 較晚的那個起算，`modified` 還是 mtime。
    #[test]
    fn a_remote_file_expires_from_the_later_of_mtime_and_ctime() {
        let out = "AM_OUTBOX_OK\n10 1000 3000\t00\tmoved-in.pdf\n10 2000 1500\t00\tclock-skew.txt\n";
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
        assert!(f.contains(r#"F="$D"/'it'\''s.md'"#) && f.contains(&format!("head -c {}", MAX_BYTES + 1)) && f.contains("3< \"$F\""), "{f}");
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
    fn run_download(dir: &std::path::Path, name: &str, gap: &str) -> String {
        let script = file_script_gap(&dir.to_string_lossy(), name, gap);
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn served(out: &str) -> Option<Vec<u8>> {
        match parse_file(out) {
            Some(Fetched::File(d)) => Some(d),
            _ => None,
        }
    }

    fn sandbox(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("am-outbox-remote-{tag}-{}", crate::db::ulid()));
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
        let out = run_download(&d, "report.txt", "");
        assert_eq!(out.trim(), "AM_OUTBOX_MISSING", "{out}");
        let d2 = base.join("outbox");
        std::fs::write(d2.join("report.txt"), b"hello").unwrap();
        let gap = format!("mv \"$D\" \"$D.real\"; ln -s '{}' \"$D\"", real.display());
        let out = run_download(&d2, "report.txt", &gap);
        assert!(!out.contains("TOP SECRET") && served(&out).as_deref() != Some(&b"TOP SECRET"[..]), "{out}");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
