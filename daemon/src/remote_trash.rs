//! 遠端 bot 目錄的回收區（issue #411）：刪 bot 時遠端的 `bots/<id>/` 不 `rm -rf`，而是 ssh 搬到同一個根底下的
//! `bots-trash/<id>.<毫秒>/`；`POST /api/bots/{id}/restore` 時 ssh 搬回來；主機連上（開機、重連）時清掉放超過
//! [`crate::bot_trash::KEEP_DAYS`] 天的。
//!
//! #406 只替本機做了回收區（`bot_trash`）。遠端誤刪後還原得回 bot 列與 config，遠端目錄裡的東西（hook spool 裡還沒重放的
//! 事件、shim、手動放的檔）卻拿不回來。規則跟本機一樣：名字裡的時間戳決定過期（`mv` 不更新目錄 mtime）；還原時
//! `bots/<id>/` 已經在（重新啟動過、重建了）就不動，免得蓋掉新的。根目錄照 SPEC §3.1 遠端分實例（`remote_root_for`）。
//! ssh 失敗照 `remote_purge` 既有的補帳：`purge_bot_dir` 記失敗，下一輪掃描再搬一次（冪等：已經不在就什麼都不做）。

use crate::hosts::{sh_quote, HostConn};
use crate::state::App;
use anyhow::{bail, Result};
use std::sync::Arc;
use std::time::Duration;

/// 還原時等 ssh 多久：在 bot 鎖裡、擋著 HTTP 回應，不用 `ssh_exec` 預設的 30 秒。
const RESTORE_TIMEOUT: Duration = Duration::from_secs(10);

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()
}

/// 這台主機、這個實例的回收區（`<home>/<root>/bots-trash`），跟 `remote_bot_dir` 同一個根。
async fn trash_dir(conn: &HostConn) -> Result<String> {
    Ok(format!("{}/{}/bots-trash", conn.home().await?, crate::startup::remote_root_for(crate::startup::instance().as_deref())))
}

/// 把遠端的 `bots/<id>/` 搬進回收區。回搬到哪裡；目錄本來就不在回 `None`。
pub async fn move_in(conn: &HostConn, bot_id: &str) -> Result<Option<String>> {
    let dir = crate::lifecycle::remote_bot_dir(conn, bot_id).await?.dir;
    let dest = format!("{}/{bot_id}.{}", trash_dir(conn).await?, now_ms());
    let script = format!(
        "set -e\nD={d}\nif [ -e \"$D\" ]; then\n  mkdir -p \"$(dirname {t})\"\n  mv \"$D\" {t}\n  printf 'AM_TRASHED\\n'\nelse\n  printf 'AM_NOTHING\\n'\nfi\n",
        d = sh_quote(&dir),
        t = sh_quote(&dest),
    );
    let out = conn.ssh_exec(&script).await?;
    if out.contains("AM_TRASHED") {
        Ok(Some(dest))
    } else if out.contains("AM_NOTHING") {
        Ok(None)
    } else {
        bail!("remote move to bots-trash did not confirm: {}", out.trim())
    }
}

/// 還原：遠端 `bots/<id>/` 還不在時，把回收區裡這顆最新的那份搬回去。回從哪裡搬回來；沒得搬回 `None`。
pub async fn restore(conn: &HostConn, bot_id: &str) -> Result<Option<String>> {
    let dir = crate::lifecycle::remote_bot_dir(conn, bot_id).await?.dir;
    let script = format!(
        "set -e\nD={d}\nT={t}\nif [ -e \"$D\" ]; then printf 'AM_KEPT\\n'; exit 0; fi\nbest=\nbm=0\nfor e in \"$T\"/{id}.*; do\n  [ -d \"$e\" ] || continue\n  ms=${{e##*.}}\n  case \"$ms\" in ''|*[!0-9]*) continue;; esac\n  if [ \"$ms\" -gt \"$bm\" ]; then bm=$ms; best=$e; fi\ndone\nif [ -z \"$best\" ]; then printf 'AM_NONE\\n'; exit 0; fi\nmkdir -p \"$(dirname \"$D\")\"\nmv \"$best\" \"$D\"\nprintf 'AM_RESTORED %s\\n' \"$best\"\n",
        d = sh_quote(&dir),
        t = sh_quote(&trash_dir(conn).await?),
        id = sh_quote(bot_id),
    );
    let out = conn.ssh_exec_timeout(&script, RESTORE_TIMEOUT).await?;
    if let Some(from) = out.lines().find_map(|l| l.strip_prefix("AM_RESTORED ")) {
        Ok(Some(from.trim().to_string()))
    } else if out.contains("AM_KEPT") || out.contains("AM_NONE") {
        Ok(None)
    } else {
        bail!("remote restore from bots-trash did not confirm: {}", out.trim())
    }
}

/// 清掉回收區裡放超過 `keep` 的（看名字裡的時間，不看 mtime）。回清掉幾份。
pub async fn gc(conn: &HostConn, keep: Duration) -> Result<usize> {
    let cutoff = now_ms().saturating_sub(keep.as_millis());
    let script = format!(
        "T={t}\nC={cutoff}\nn=0\nif [ -d \"$T\" ]; then\n  for e in \"$T\"/*.*; do\n    [ -d \"$e\" ] || continue\n    ms=${{e##*.}}\n    case \"$ms\" in ''|*[!0-9]*) continue;; esac\n    if [ \"$ms\" -le \"$C\" ]; then rm -rf \"$e\" && n=$((n+1)); fi\n  done\nfi\nprintf 'AM_TRASH_GC %s\\n' \"$n\"\n",
        t = sh_quote(&trash_dir(conn).await?),
    );
    let out = conn.ssh_exec(&script).await?;
    out.lines()
        .find_map(|l| l.strip_prefix("AM_TRASH_GC "))
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("remote bots-trash gc did not confirm: {}", out.trim()))
}

/// `restore_bot` 用：bot 在遠端就把目錄搬回來，並忘掉「已清掉」的記號——之後再刪一次，掃描才會再搬它。
/// 搬不回來不擋還原（下次啟動會重建需要的檔；回收區那份留到過期），只記 log。
pub async fn restore_for(app: &Arc<App>, bot_id: &str) {
    let host = match crate::db::bot_host(&app.db, bot_id).await {
        Ok(h) if h != crate::config::LOCAL_HOST => h,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(bot = %bot_id, error = %e, "could not read the bot's host; remote bots-trash not restored");
            return;
        }
    };
    crate::remote_purge::forget(app, bot_id).await;
    let Some(conn) = app.hosts.get(&host).await else {
        tracing::warn!(host, bot = %bot_id, "unknown host; remote bot dir not restored from bots-trash");
        return;
    };
    match restore(&conn, bot_id).await {
        Ok(Some(from)) => tracing::info!(host, bot = %bot_id, %from, "restored remote bot config dir from bots-trash"),
        Ok(None) => {}
        Err(e) => tracing::warn!(host, bot = %bot_id, error = %format!("{e:#}"), "could not restore remote bot config dir from bots-trash"),
    }
}

/// 主機連上時清一次過期的。
pub async fn gc_host(app: &Arc<App>, host: &str) {
    let Some(conn) = app.hosts.get(host).await else { return };
    if conn.is_local() {
        return;
    }
    match gc(&conn, Duration::from_secs(crate::bot_trash::KEEP_DAYS * 86_400)).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(host, removed = n, "removed expired remote bots-trash entries"),
        Err(e) => tracing::warn!(host, error = %format!("{e:#}"), "could not clean expired remote bots-trash entries"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    /// 假 ssh：腳本真的交給本機 `/bin/sh` 跑，遠端家目錄是測試自己的暫存目錄——搬的、清的都是真的檔案。
    fn run_sh(script: &str) -> Result<String> {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(script.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("sh failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// 專案在遠端主機 `host`（每個測試自己的名字：ssh 假貨是全域的），家目錄在暫存目錄。
    async fn remote(host: &'static str) -> (tt::Env, PathBuf) {
        let env = tt::env().await;
        let home = env.dir.join("remote-home");
        std::fs::create_dir_all(&home).unwrap();
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new() };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
        sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(host).bind(&env.project_id).execute(&env.app.db).await.unwrap();
        crate::hosts::set_ssh_fake(host, run_sh);
        let root = home.join(crate::startup::remote_root_for(crate::startup::instance().as_deref()));
        (env, root)
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        v.sort();
        v
    }

    /// #411：刪除把遠端目錄搬進回收區（不是 `rm -rf`），還原搬回來、內容原樣，「已清掉」的記號也忘掉。
    #[tokio::test]
    async fn a_deleted_remote_bot_dir_goes_to_the_trash_and_comes_back_on_restore() {
        let (env, root) = remote("trashbox-roundtrip").await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        // child：還原只清 `deleted_at`，不必動 config.toml。
        sqlx::query("UPDATE bots SET managed_by = 'child' WHERE id = ?").bind(&bot.id).execute(&app.db).await.unwrap();
        let dir = root.join("bots").join(&bot.id);
        std::fs::create_dir_all(dir.join("spool")).unwrap();
        std::fs::write(dir.join("spool/ev.json"), "pending").unwrap();

        assert!(crate::lifecycle::purge_bot_dir(&app, &bot.id, "trashbox-roundtrip").await);
        assert!(!dir.exists(), "搬走了");
        let trashed = entries(&root.join("bots-trash"));
        assert_eq!(trashed.len(), 1, "{trashed:?}");
        assert!(trashed[0].starts_with(&format!("{}.", bot.id)), "{trashed:?}");
        let purged: Option<String> = sqlx::query_scalar("SELECT purged_at FROM remote_bot_dir_purges WHERE bot_id = ?").bind(&bot.id).fetch_one(&app.db).await.unwrap();
        assert!(purged.is_some(), "照舊記下已處理");

        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(crate::db::now()).bind(&bot.id).execute(&app.db).await.unwrap();
        crate::api::restore_bot(axum::extract::State(app.clone()), axum::extract::Path(bot.id.clone())).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("spool/ev.json")).unwrap(), "pending", "還原 API 把 spool 裡的東西拿回來");
        assert!(entries(&root.join("bots-trash")).is_empty());
        let row: Option<String> = sqlx::query_scalar("SELECT bot_id FROM remote_bot_dir_purges WHERE bot_id = ?").bind(&bot.id).fetch_optional(&app.db).await.unwrap();
        assert!(row.is_none(), "還原後忘掉記號：再刪一次，掃描才會再搬");

        // 還原時目錄已經在（重建過）就不動回收區那份。
        assert!(crate::lifecycle::purge_bot_dir(&app, &bot.id, "trashbox-roundtrip").await);
        std::fs::create_dir_all(&dir).unwrap();
        restore_for(&app, &bot.id).await;
        assert_eq!(entries(&root.join("bots-trash")).len(), 1, "不蓋掉已經在的目錄");
        assert!(!dir.join("spool").exists());

        // 目錄本來就不在：什麼都不搬，照樣算處理完。
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(crate::lifecycle::purge_bot_dir(&app, &bot.id, "trashbox-roundtrip").await);
        assert_eq!(entries(&root.join("bots-trash")).len(), 1);
    }

    /// 同一顆被刪過好幾次：還原拿最新那份（名字裡的毫秒最大）。glob 是字典序（900 排最後、1000 排最前），頭尾都不是最新的。
    #[tokio::test]
    async fn restore_takes_the_newest_copy() {
        let (env, root) = remote("trashbox-newest").await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let trash = root.join("bots-trash");
        for (ms, tag) in [("900", "oldest"), ("1000", "old"), ("2000", "new")] {
            std::fs::create_dir_all(trash.join(format!("{}.{ms}", bot.id))).unwrap();
            std::fs::write(trash.join(format!("{}.{ms}/tag", bot.id)), tag).unwrap();
        }
        restore_for(&app, &bot.id).await;
        assert_eq!(std::fs::read_to_string(root.join("bots").join(&bot.id).join("tag")).unwrap(), "new");
    }

    /// 主機連上時清掉放超過保留期的；新的、名字不合規矩的都不碰。
    #[tokio::test]
    async fn expired_trash_entries_are_removed_and_fresh_ones_kept() {
        let (env, root) = remote("trashbox-gc").await;
        let conn = env.app.hosts.get("trashbox-gc").await.unwrap();
        let trash = root.join("bots-trash");
        let old = now_ms() - Duration::from_secs(8 * 86_400).as_millis();
        let fresh = now_ms() - Duration::from_secs(86_400).as_millis();
        for name in [format!("aaa.{old}"), format!("bbb.{fresh}"), "notes.txt.bak".into(), "ccc".into()] {
            std::fs::create_dir_all(trash.join(name)).unwrap();
        }
        assert_eq!(gc(&conn, Duration::from_secs(crate::bot_trash::KEEP_DAYS * 86_400)).await.unwrap(), 1);
        assert_eq!(entries(&trash), vec!["bbb.".to_string() + &fresh.to_string(), "ccc".into(), "notes.txt.bak".into()]);
        gc_host(&env.app, "trashbox-gc").await;
        assert_eq!(entries(&trash).len(), 3, "沒有過期的就不動");
    }

    /// ssh 失敗：刪除記成欠著（`remote_purge` 補帳），目錄原地不動。
    #[tokio::test]
    async fn a_failed_move_stays_owed() {
        let (env, root) = remote("trashbox-down").await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let dir = root.join("bots").join(&bot.id);
        std::fs::create_dir_all(&dir).unwrap();
        crate::hosts::set_ssh_fake("trashbox-down", |_| bail!("ssh: connect to host: Connection refused"));
        assert!(!crate::lifecycle::purge_bot_dir(&app, &bot.id, "trashbox-down").await);
        assert!(dir.exists());
        let (purged, attempts): (Option<String>, i64) =
            sqlx::query_as("SELECT purged_at, attempts FROM remote_bot_dir_purges WHERE bot_id = ?").bind(&bot.id).fetch_one(&app.db).await.unwrap();
        assert!(purged.is_none() && attempts == 1);
    }
}
