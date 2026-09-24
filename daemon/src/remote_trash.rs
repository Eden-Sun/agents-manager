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

/// 兩道一起跑，跟本機 [`crate::bot_trash::gc_with_cap`] 同一套政策（#441）：先清放超過 `keep` 的
/// （看名字裡的時間，不看 mtime——`mv` 不更新目錄 mtime），再看總量，還超過 `max_bytes` 就從**最舊的**
/// 開始清到降下來；最新那一份永遠留著（剛刪掉的那顆才是最可能要還原的）。回 `(過期清掉幾份, 因為超量再清掉幾份)`。
///
/// 只有時間規則不夠：#141／#196 那兩次遠端磁碟被塞爆都是「清得不夠快」，而七天之內連刪十幾顆 bot 時，
/// 時間規則一份都不會清。
///
/// **跟 [`restore`] 不互斥**：`restore` 挑中一份剛好跨過期限的、而這裡在它 `mv` 之前就 `rm -rf` 掉，
/// `restore` 的 `set -e` 會讓腳本失敗、[`restore_for`] 記一行 warn 不擋還原。窗口極窄（那一份本來下一輪
/// 也要過期），代價是那個目錄拿不回來，不值得為它加鎖。
pub async fn gc(conn: &HostConn, keep: Duration, max_bytes: u64) -> Result<(usize, usize)> {
    let cutoff = now_ms().saturating_sub(keep.as_millis());
    let max_kb = max_bytes / 1024;
    // basename 是 `move_in` 造的 `<bot_id>.<毫秒>`（ULID，不含空白），所以第二道可以用行為單位排序；
    // 不是這個形狀的一律不動——那不是我們放的。`$T` 本身有空白也沒關係，只有 basename 進 sort。
    let script = format!(
        "T={t}\nC={cutoff}\nMAXK={max_kb}\nn=0\nev=0\n\
         # 列出「我們放的、可以按大小淘汰的」：<毫秒> <KB> <basename>。抽成函式是因為 `case` 的樣式\n\
         # 帶 `)`，寫在 $(...) 裡會被 shell 當成命令替換的結尾（macOS /bin/sh 實測語法錯誤）。\n\
         am_list() {{\n\
        \x20 for d in \"$T\"/*.*; do\n\
        \x20   [ -d \"$d\" ] || continue\n\
        \x20   b=${{d##*/}}; ms=${{b##*.}}\n\
        \x20   case \"$ms\" in ''|*[!0-9]*) continue;; esac\n\
        \x20   case \"$b\" in *[[:space:]]*) continue;; esac\n\
        \x20   printf '%s %s %s\\n' \"$ms\" \"$(du -sk \"$d\" 2>/dev/null | awk 'NR==1{{print $1+0}}')\" \"$b\"\n\
        \x20 done\n\
         }}\n\
         if [ -d \"$T\" ]; then\n\
        \x20 for d in \"$T\"/*.*; do\n\
        \x20   [ -d \"$d\" ] || continue\n\
        \x20   b=${{d##*/}}; ms=${{b##*.}}\n\
        \x20   case \"$ms\" in ''|*[!0-9]*) continue;; esac\n\
        \x20   if [ \"$ms\" -le \"$C\" ]; then rm -rf \"$d\" && n=$((n+1)); fi\n\
        \x20 done\n\
        \x20 list=$(am_list | sort -n)\n\
        \x20 total=$(printf '%s\\n' \"$list\" | awk '{{s+=$2}} END{{print s+0}}')\n\
        \x20 cnt=$(printf '%s\\n' \"$list\" | grep -c '[^[:space:]]')\n\
        \x20 i=0\n\
        \x20 for b in $(printf '%s\\n' \"$list\" | awk '{{print $3}}'); do\n\
        \x20   i=$((i+1))\n\
        \x20   [ \"$i\" -lt \"$cnt\" ] || break\n\
        \x20   [ \"$total\" -gt \"$MAXK\" ] || break\n\
        \x20   k=$(du -sk \"$T/$b\" 2>/dev/null | awk 'NR==1{{print $1+0}}')\n\
        \x20   if rm -rf \"$T/$b\"; then ev=$((ev+1)); total=$((total-k)); fi\n\
        \x20 done\n\
         fi\n\
         printf 'AM_TRASH_GC %s %s\\n' \"$n\" \"$ev\"\n",
        t = sh_quote(&trash_dir(conn).await?),
    );
    let out = conn.ssh_exec(&script).await?;
    out.lines()
        .find_map(|l| l.strip_prefix("AM_TRASH_GC "))
        .and_then(|rest| {
            let mut it = rest.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
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
    // 保留期與總量上限都跟本機同一個值（#441）：同一套政策，兩邊不必分開記。
    match gc(&conn, Duration::from_secs(crate::bot_trash::KEEP_DAYS * 86_400), crate::bot_trash::MAX_BYTES).await {
        Ok((0, 0)) => {}
        Ok((expired, evicted)) => {
            if evicted > 0 {
                tracing::warn!(host, expired, evicted, "remote bots-trash is over its size cap; removed the oldest entries");
            } else {
                tracing::info!(host, expired, "removed expired remote bots-trash entries");
            }
        }
        Err(e) => tracing::warn!(host, error = %format!("{e:#}"), "could not clean the remote bots-trash"),
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
        assert_eq!(gc(&conn, Duration::from_secs(crate::bot_trash::KEEP_DAYS * 86_400), crate::bot_trash::MAX_BYTES).await.unwrap(), (1, 0));
        assert_eq!(entries(&trash), vec!["bbb.".to_string() + &fresh.to_string(), "ccc".into(), "notes.txt.bak".into()]);
        gc_host(&env.app, "trashbox-gc").await;
        assert_eq!(entries(&trash).len(), 3, "沒有過期的就不動");
    }

    /// 在回收區種一份 `<id>.<ms>/`，裡面放 `kb` KB 的內容（`du -sk` 量得到的才算數）。
    fn seed(trash: &Path, id: &str, ms: u128, kb: usize) -> PathBuf {
        let d = trash.join(format!("{id}.{ms}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("blob"), vec![b'x'; kb * 1024]).unwrap();
        d
    }

    /// **#441**：時間規則擋不住「七天之內連刪十幾顆」——那正是 #141／#196 塞爆遠端磁碟的形狀。
    /// 跟本機 `bot_trash::the_oldest_entries_go_first_once_the_trash_is_over_its_size_cap` 同一套：
    /// 沒超量一個都不動；超量就從最舊的清到降下來，最新那一份永遠留著。
    #[tokio::test]
    async fn the_oldest_remote_entries_go_first_once_the_trash_is_over_its_size_cap() {
        let (env, root) = remote("trashbox-cap").await;
        let conn = env.app.hosts.get("trashbox-cap").await.unwrap();
        let trash = root.join("bots-trash");
        let now = now_ms();
        let keep_long = Duration::from_secs(3600);
        let (old, mid, new) = (seed(&trash, "b1", now - 3_000, 64), seed(&trash, "b2", now - 2_000, 64), seed(&trash, "b3", now - 1_000, 64));

        // 沒超量、也沒過期：一個都不動。
        assert_eq!(gc(&conn, keep_long, 100 * 1024 * 1024).await.unwrap(), (0, 0));
        assert!(old.exists() && mid.exists() && new.exists());

        // 上限只容得下一份：最舊的兩份走，最新那份留著。
        assert_eq!(gc(&conn, keep_long, 100 * 1024).await.unwrap(), (0, 2));
        assert!(!old.exists() && !mid.exists(), "最舊的先清");
        assert!(new.exists(), "最新那一份永遠留著");
    }

    /// 過期的先清；清完還超量才輪到按大小淘汰，兩個數字分開回報（同本機 `expiry_runs_before_the_size_cap`）。
    #[tokio::test]
    async fn remote_expiry_runs_before_the_size_cap() {
        let (env, root) = remote("trashbox-both").await;
        let conn = env.app.hosts.get("trashbox-both").await.unwrap();
        let trash = root.join("bots-trash");
        let now = now_ms();
        seed(&trash, "b1", now - 10_000, 64); // 過期
        let mid = seed(&trash, "b2", now - 2_000, 64);
        let new = seed(&trash, "b3", now - 1_000, 64);

        assert_eq!(gc(&conn, Duration::from_millis(5_000), 100 * 1024).await.unwrap(), (1, 1));
        assert!(!mid.exists() && new.exists());
        assert_eq!(entries(&trash), vec![new.file_name().unwrap().to_string_lossy().into_owned()]);
    }

    /// 名字看不懂的（別人放進來的檔案、暫存目錄）一律不碰，兩道都一樣（同本機
    /// `entries_with_unparseable_names_are_never_touched`）。
    #[tokio::test]
    async fn remote_entries_with_unparseable_names_are_never_touched() {
        let (env, root) = remote("trashbox-alien").await;
        let conn = env.app.hosts.get("trashbox-alien").await.unwrap();
        let trash = root.join("bots-trash");
        std::fs::create_dir_all(trash.join("not-a-trash-entry")).unwrap();
        std::fs::write(trash.join("README"), "x").unwrap();

        assert_eq!(gc(&conn, Duration::ZERO, 0).await.unwrap(), (0, 0));
        assert_eq!(entries(&trash), vec!["README".to_string(), "not-a-trash-entry".into()]);
    }

    /// 名字帶空白的（只可能是人手動放的，`move_in` 造的是 `<ULID>.<毫秒>`）：第二道的排序以行為單位，
    /// 這種跳過——不算進總量、也不會被淘汰。第一道（過期）跟本機一樣只看那串毫秒，這裡用很長的 `keep`
    /// 把它排除，單獨釘住第二道的行為。
    #[tokio::test]
    async fn a_remote_entry_whose_name_has_spaces_is_never_evicted_for_size() {
        let (env, root) = remote("trashbox-space").await;
        let conn = env.app.hosts.get("trashbox-space").await.unwrap();
        let trash = root.join("bots-trash");
        let now = now_ms();
        let spaced = trash.join(format!("has space.{}", now - 9_000));
        std::fs::create_dir_all(&spaced).unwrap();
        std::fs::write(spaced.join("blob"), vec![b'x'; 64 * 1024]).unwrap();
        let ours = seed(&trash, "b1", now - 1_000, 64);

        // 上限 0：我們自己的只剩最新那一份（永遠留著），帶空白的那個一個位元組都沒被動到。
        assert_eq!(gc(&conn, Duration::from_secs(3600), 0).await.unwrap(), (0, 0));
        assert!(spaced.exists() && ours.exists());
    }

    /// **#431**：清理不能只掛在「主機連上」那一次。常駐連線的主機不會再連一次，以前就永遠不清、
    /// `KEEP_DAYS` 等於沒生效。這裡走的是每 5 分鐘那一輪真正呼叫的那支（`remote_purge::sweep`），
    /// 主機從頭到尾沒有重連、也沒有任何 bot 被刪（`pending` 是空的），過期的那份還是要消失。
    #[tokio::test]
    async fn a_host_that_never_reconnects_still_has_its_expired_trash_cleaned() {
        let (env, root) = remote("trashbox-poll").await;
        let trash = root.join("bots-trash");
        let old = now_ms() - Duration::from_secs((crate::bot_trash::KEEP_DAYS + 1) * 86_400).as_millis();
        let fresh = now_ms() - Duration::from_secs(86_400).as_millis();
        std::fs::create_dir_all(trash.join(format!("aaa.{old}"))).unwrap();
        std::fs::create_dir_all(trash.join(format!("bbb.{fresh}"))).unwrap();

        // 5 分鐘那一輪對每台已連線的遠端主機做的事，就是這一行（`remote_purge::spawn_poller`）。
        assert_eq!(crate::remote_purge::sweep(&env.app, "trashbox-poll").await, (0, 0), "沒有欠著的目錄");

        assert_eq!(entries(&trash), vec!["bbb.".to_string() + &fresh.to_string()], "過期的清掉、沒過期的留著");
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
