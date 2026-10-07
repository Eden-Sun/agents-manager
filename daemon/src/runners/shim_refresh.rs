//! Remote shim refresh runner.

use crate::shim_refresh::*;
use serde_json::json;
use std::sync::Arc;

/// 開機時跑一次：換掉的記一行 info，換不動的每一支推一則 inbox 給巡檢。
pub async fn refresh_at_startup(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db)) {
    let out = refresh_all(app.data_dir());
    if out.changed.is_empty() {
        tracing::debug!("every bot shim is already the version this binary carries");
    } else {
        tracing::info!(count = out.changed.len(), shims = ?out.changed, "refreshed bot shims to this binary's version (no pane restart needed)");
    }
    for (who, error) in &out.failed {
        let (bot_id, shim) = who.split_once('/').unwrap_or((who.as_str(), ""));
        let wanted = shims().iter().find(|(n, _)| *n == shim).map(|(_, c)| *c).unwrap_or_default();
        // event_key 帶內容雜湊：同一個版本換不動只會有一則（`push_inbox` 是 INSERT OR IGNORE），
        // 下一顆 binary 帶了新 shim 又失敗才是新的一則。
        let key = format!("{SHIM_STALE_KIND}:{who}:{}", crate::supervisor_inbox::short_hash(wanted.as_bytes()));
        let _ = crate::supervisor_inbox::push_inbox(
            app.db(),
            &key,
            SHIM_STALE_KIND,
            None,
            Some(bot_id),
            None,
            &json!({
                "bot_id": bot_id,
                "shim": shim,
                "path": app.data_dir().join("bots").join(bot_id).join("bin").join(shim).to_string_lossy(),
                "embedded_hash": crate::supervisor_inbox::short_hash(wanted.as_bytes()),
                "error": error,
                "action": "這顆 bot 手上還是舊 shim（舊 cargo shim ＝ 工作不會被轉到外部編譯主機，還可能跟 build shim 互相當成真 cargo 而卡住）：修好那個檔案的權限／磁碟，下一次 daemon 重啟會再換一次；急的話重啟這顆 bot 的 pane 也會重寫",
            }),
        )
        .await;
    }
}


/// 連上（或重連）一台遠端 host 之後：盤點這台上還受管理的 bot（`live_bots_on_host`），把它們**已經存在**的
/// `herdr`／`cargo` shim 換成這顆 binary 帶的版本，不必重啟任何 pane。回傳換了什麼。
///
/// 從沒 setup 過的 bot（沒有 `bin/` 或沒有那支 shim）不會被生出新檔案——那是啟動時 `install_shim` 的事。
pub(crate) async fn refresh_remote_host(app: &(impl crate::capabilities::Db + crate::hosts::HostInstance + crate::hosts::HostsAccess), host: &str) -> anyhow::Result<RemoteSync> {
    let conn = app.hosts().get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    if conn.is_local() {
        return Ok(RemoteSync::default());
    }
    if !conn.is_connected() {
        anyhow::bail!("host `{host}` is not connected");
    }
    let bots = crate::db::live_bots_on_host(app.db(), host).await?;
    let mut dirs = Vec::new();
    for b in &bots {
        dirs.push(crate::app_ports_p12::remote_bot_dir_for(&conn, &b.id, app.instance().as_deref()).await?);
    }
    if dirs.is_empty() {
        return Ok(RemoteSync::default());
    }
    let names: Vec<&str> = shims().iter().map(|(n, _)| *n).collect();
    sync_remote(&conn, &dirs, &names, false).await
}

/// 遠端補版的重試節奏（issue #534）：立刻、5 分鐘、15 分鐘、60 分鐘後。
///
/// 以前是 20 秒與 60 秒——那只夠撐過「ssh 剛好抖一下」。真的會讓補版失敗的是那台在重開機、
/// 網路斷了幾分鐘、sshd 還沒起來、磁碟滿了有人正在清，三次加起來 80 秒全都趕不上，
/// 而補版的下一次機會要等這台**掉線再連上**（一直連著就等到 daemon 重啟）。
/// 補版沒成的原因記在 `app.remote_shim_stale`，由 `supervisor::incidents` 開票（issue #534）。
async fn note_stale(app: &impl crate::shim_refresh::RemoteShimStale, host: &str, why: String) {
    app.remote_shim_stale().lock().await.insert(host.to_string(), why);
}

async fn clear_stale(app: &impl crate::shim_refresh::RemoteShimStale, host: &str) {
    app.remote_shim_stale().lock().await.remove(host);
}

/// host 連上之後在背景補版：**不擋連線、不擋 daemon 啟動**。失敗（ssh 抖了、host 在重開機）就按
/// [`REMOTE_RETRY_WAITS`] 退避重試；host 那時已不在線就放棄——下一次 supervisor 連上會再叫一次
/// （補版是冪等的）。**重試用完、或有檔案換不動就開 incident**：這台上的遠端 bot 手上還是舊 shim，
/// 而它不會自己好，也沒有別的探針會發現（`resource` 是 host 名）。
pub(crate) fn spawn_remote_refresh(app: Arc<impl crate::capabilities::Db + crate::hosts::HostInstance + crate::hosts::HostsAccess + crate::shim_refresh::RemoteShimStale + 'static>, host: String) {
    tokio::spawn(async move {
        let mut last_err = String::new();
        for (i, wait) in REMOTE_RETRY_WAITS.iter().enumerate() {
            if *wait > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(*wait)).await;
            }
            match app.hosts().get(&host).await {
                Some(c) if c.is_connected() => {}
                // 掉線了：這一輪不算數，也不開票——連上時會再叫一次。
                _ => return,
            }
            match refresh_remote_host(&app, &host).await {
                Ok(r) if r.updated.is_empty() && r.failed.is_empty() => {
                    tracing::debug!(host = %host, "every remote bot shim is already the version this binary carries");
                    clear_stale(&app, &host).await;
                    return;
                }
                Ok(r) => {
                    if !r.updated.is_empty() {
                        tracing::info!(host = %host, count = r.updated.len(), shims = ?r.updated, "refreshed remote bot shims to this binary's version (no pane restart needed)");
                    }
                    if r.failed.is_empty() {
                        clear_stale(&app, &host).await;
                    } else {
                        // 腳本跑完了但某幾支 cp／mv 失敗（磁碟滿、權限、被改成目錄）：重試同一輪也是一樣的結果，
                        // 開票交給人處理，別把 ssh 再打三次。
                        tracing::warn!(host = %host, shims = ?r.failed, "could not refresh some remote shims (old files untouched)");
                        note_stale(&app, &host, format!("這幾支換不動（舊檔原封不動）：{:?}", r.failed)).await;
                    }
                    return;
                }
                Err(e) => {
                    last_err = e.to_string();
                    if i + 1 < REMOTE_RETRY_WAITS.len() {
                        tracing::warn!(host = %host, attempt = i + 1, error = %e, "could not refresh remote bot shims; will retry");
                    }
                }
            }
        }
        // 走到這裡＝每一次都失敗。以前最後一行寫的也是 "will retry"，但迴圈已經結束——
        // 那是唯一留下的紀錄，看的人會以為還有下一次（issue #534）。
        tracing::warn!(
            host = %host,
            attempts = REMOTE_RETRY_WAITS.len(),
            error = %last_err,
            "gave up refreshing remote bot shims; this host's bots keep running the old shim until it reconnects or the daemon restarts"
        );
        note_stale(&app, &host, format!("重試 {} 次都失敗，最後一個錯誤：{last_err}", REMOTE_RETRY_WAITS.len())).await;
    });
}
