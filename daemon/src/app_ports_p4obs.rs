//! P4 am-turn-observe 子群組（p4obs）的 App 端接縫 adapter。
//!
//! 封裝 poller、screen、transcript_origin、transcript_stage、grok_transcript、
//! claude_linux_screens、codex_banner、limit_banner 等模組對 App、db、quota、
//! supervisor、events、attach、turn_error 的直接依賴。

#![allow(dead_code)]

use std::sync::Arc;
use crate::db;
use crate::state::App;
use anyhow::Result;

// --- Screen & Cursor Bookkeeping ---

/// 更新 runs 表的 pane cursor。
pub async fn remember_pane_cursor(app: &impl crate::capabilities::Db, run_id: &str, revision: i64, tail_hash: &str) -> Result<()> {
    sqlx::query("UPDATE runs SET last_read_revision=?, last_read_tail_hash=? WHERE id=?")
        .bind(revision)
        .bind(tail_hash)
        .bind(run_id)
        .execute(app.db())
        .await?;
    Ok(())
}

/// 查詢對話訊息總數。
pub async fn conversation_message_count(app: &impl crate::capabilities::Db, conversation_id: &str) -> Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
        .bind(conversation_id)
        .fetch_one(app.db())
        .await?)
}

/// 查詢最新 assistant 訊息內容。
pub async fn last_assistant_content(app: &impl crate::capabilities::Db, conversation_id: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT content FROM messages WHERE conversation_id = ? AND role = 'assistant'
         ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(app.db())
    .await?)
}

/// 讀取 pane recent unwrapped 文字。
pub async fn read_pane_recent_unwrapped(
    app: &impl crate::capabilities::HerdrRoutes,
    run: &db::Run,
    pane_id: &str,
    lines: u32,
) -> Result<Option<crate::herdr::PaneRead>> {
    let Some(client) = app.herdr_for_run(run).await else { return Ok(None) };
    let read = client.pane_read(pane_id, "recent_unwrapped", lines).await?;
    Ok(Some(read))
}

/// 將 system notice 存入 messages 表。
pub async fn insert_system_message(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), conversation_id: &str, content: &str) -> Result<()> {
    crate::lifecycle::insert_message(app, conversation_id, None, "system", content, "system", false, None).await?;
    Ok(())
}

/// 記錄 shadow limit hit（裁判）。
pub async fn shadow_limit_hit(app: &Arc<App>, sample: crate::judge::Sample) {
    crate::judge::shadow_limit_hit(app, sample).await;
}

/// 標註 codex limit hit 錯誤。
pub async fn mark_codex_limit_hit(app: &Arc<App>, bot: &db::Bot, notice: &str) -> Result<()> {
    crate::turn_error::mark_codex_limit_hit(app, bot, notice).await
}

/// 結束 in-flight 回合為失敗。
pub async fn fail_in_flight_turn(app: &Arc<App>, turn_id: &str, reason: &str) -> Result<()> {
    let res = crate::lifecycle::turn_controller::fail(&app.db, turn_id, crate::lifecycle::turn_controller::DeliveryOnFail::Keep, reason).await?;
    if res == crate::lifecycle::turn_controller::Outcome::Applied {
        crate::lifecycle::emit_turn(app, turn_id).await;
    }
    Ok(())
}

/// 將撞限橫幅寫入額度那一格（apply_codex_limit_hit_quota）。
pub async fn apply_codex_limit_hit_quota(
    app: &Arc<App>,
    host: &str,
    base: &str,
    hit: crate::quota::LimitHit,
) -> crate::quota::LimitHit {
    let key = crate::quota::quota_key(host, base);
    let mut q = app
        .quotas
        .lock()
        .await
        .get(&key)
        .cloned()
        .unwrap_or_else(|| crate::quota::Quota {
            five_hour: None,
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "codex-limit-hit".into(),
            account: None,
            host: host.to_string(),
        });
    if let Some(h) = q.limit_hit.as_ref().filter(|h| h.message == hit.message && !crate::quota::limit_hit_expired(Some(h))) {
        return h.clone();
    }
    let win = crate::quota::Window { observed_at: None, used_pct: 100.0, resets_at: None };
    if let Some(existing) = q.five_hour.as_mut() {
        existing.used_pct = 100.0;
    } else if let Some(existing) = q.seven_day.as_mut() {
        existing.used_pct = 100.0;
    } else {
        q.five_hour = Some(win);
    }
    q.limit_hit = Some(hit.clone());
    q.updated_at = crate::db::now();
    q.source = "codex-limit-hit".into();
    crate::quota::set(app, host, base, q).await;
    hit
}

// --- Codex Banner Adapter ---

/// 發送 codex 安全提醒通知訊息至聊天室。
pub async fn post_codex_security_banner_notice(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), run: &db::Run, notice: &str) {
    match db::conversation_id(app.db(), &run.bot_id).await {
        Ok(conv) => {
            if let Err(e) = crate::lifecycle::insert_message(app, &conv, None, "system", notice, "system", false, None).await {
                tracing::warn!(run = %run.id, error = ?e, "could not post the codex security banner notice");
            }
        }
        Err(e) => tracing::warn!(run = %run.id, error = ?e, "could not post the codex security banner notice"),
    }
}

/// 推送 codex 安全提醒 ops_alert 到 supervisor inbox。
pub async fn push_codex_security_banner_alert(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), run: &db::Run, reason: &str) {
    let name = match db::bot(app.db(), &run.bot_id).await {
        Ok(Some(b)) => b.name,
        _ => String::new(),
    };
    let subject = if name.is_empty() { run.bot_id.as_str() } else { name.as_str() };
    let key = format!("ops_alert:daemon:{reason}:{}:{}", run.id, db::ulid());
    let payload = serde_json::json!({
        "source": "daemon",
        "reason": reason,
        "subject": subject,
        "bot_id": run.bot_id,
        "bot_name": name,
        "run_id": run.id,
        "detail": format!(
            "codex bot `{name}`（{}）畫面上有帳號安全提醒橫幅（`Press a number to choose`）：橫幅開著時打字，開頭的數字會被當成選項吃掉，daemon 已擋下派送（一個字都沒打）",
            run.bot_id
        ),
        "action": "請人到這顆 bot 的「終端」pane 處理橫幅（選一個選項，或按 Esc 關掉），再重送；排隊的訊息會在橫幅關掉後自動重試。daemon 不會替你按 Esc 或任何鍵。同一次橫幅只推這一則，關掉後又出現才再推。",
    });
    match crate::supervisor::store::push_inbox(app.db(), &key, "ops_alert", None, Some(&run.bot_id), None, &payload).await {
        Ok(Some(_)) => app.emit("supervisor_changed", serde_json::json!({ "ops_alert": key })).await,
        Ok(None) => {}
        Err(e) => tracing::warn!(run = %run.id, error = %e, "could not queue the codex security banner ops_alert"),
    }
}

// --- Transcript Origin Adapter ---

struct AppTranscriptRoots<'a> {
    app: &'a Arc<App>,
    bot: &'a db::Bot,
}

impl crate::transcript_read::TranscriptRoots for AppTranscriptRoots<'_> {
    fn kind(&self) -> &str {
        &self.bot.kind
    }

    fn is_local(&self) -> impl std::future::Future<Output = Option<bool>> + Send + '_ {
        async move { db::bot_host(&self.app.db, &self.bot.id).await.ok().map(|host| host == crate::config::LOCAL_HOST) }
    }

    fn claude_config_dir(&self) -> impl std::future::Future<Output = Option<String>> + Send + '_ {
        async move {
            let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned());
            match (self.bot.env().get("CLAUDE_CONFIG_DIR"), &home) {
                (Some(v), Some(h)) if !v.trim().is_empty() => Some(crate::config::expand_home(v.trim(), h)),
                _ => crate::lifecycle::identity_config_dir(self.app, crate::config::LOCAL_HOST, self.bot.identity.as_deref()).await.ok(),
            }
        }
    }

    fn codex_home(&self) -> impl std::future::Future<Output = Option<std::path::PathBuf>> + Send + '_ {
        async move { crate::lifecycle::codex_home(self.app, self.bot).await }
    }

    fn user_home(&self) -> Option<std::path::PathBuf> {
        crate::home::dir()
    }
}

/// 檢查此本機/遠端 transcript 是否為 CLI 自起。
pub async fn started_by_the_cli_itself(app: &Arc<App>, bot: &db::Bot, transcript_path: Option<&str>) -> bool {
    let roots = AppTranscriptRoots { app, bot };
    crate::lifecycle::transcript_origin::started_by_the_cli_itself_with(&roots, &bot.kind, transcript_path).await
}

// --- Poller & Turn Error Adapters ---

/// 清除 turn error。
pub async fn clear_turn_error(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit + crate::login_prompt::LoginNeeded), run_id: &str, bot_id: &str) -> Result<()> {
    crate::turn_error::clear(app, run_id, bot_id).await
}

/// 檢查是否重複已回答的 prompt。
pub async fn repeats_answered_prompt(
    conn: &mut sqlx::SqliteConnection,
    conv: &str,
    turn_id: &str,
    text: &str,
) -> Result<bool> {
    crate::hookrecv::repeats_answered_prompt(conn, conv, turn_id, text).await
}

/// 重建或讀取 turn 的 prompt texts 與 attachment 衍生交付文字。
pub async fn turn_echo_texts(app: &impl crate::capabilities::Db, turn_id: &str) -> Result<Vec<String>> {
    let rows = db::turn_user_messages_with_attachments(app.db(), turn_id).await?;
    let mut out = Vec::new();
    let delivered: Option<String> = sqlx::query_scalar("SELECT prompt_text FROM turns WHERE id = ?")
        .bind(turn_id)
        .fetch_optional(app.db())
        .await?
        .flatten();
    if let Some(p) = delivered.filter(|p| !p.trim().is_empty()) {
        out.push(p);
    }
    for (content, attachments) in rows {
        if let Some(json) = attachments.as_deref() {
            if let Ok(items) = serde_json::from_str::<Vec<crate::attach::Attachment>>(json) {
                let delivered = crate::attach::deliver_text(&content, &items);
                if delivered != content && !out.contains(&delivered) {
                    out.push(delivered);
                }
            }
        }
        if !out.contains(&content) {
            out.push(content);
        }
    }
    Ok(out)
}

// --- Grok Transcript Host & Process Adapters ---

/// 在 host 上執行 script。
pub async fn host_sh(app: &impl crate::hosts::HostsAccess, host: &str, script: &str) -> Result<String> {
    let conn = app.hosts().get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    if conn.is_local() {
        let o = crate::local_sh::output(script).await?;
        if !o.status.success() {
            anyhow::bail!("sh exited {}", o.status);
        }
        return Ok(String::from_utf8_lossy(&o.stdout).into_owned());
    }
    if !conn.is_connected() {
        anyhow::bail!("host `{host}` is not connected");
    }
    conn.ssh_exec_path(script).await
}

/// 解析 grok home 路徑。
pub async fn grok_home_for(app: &impl crate::hosts::HostsAccess, bot: &db::Bot, host: &str) -> Result<String> {
    let env: serde_json::Value = serde_json::from_str(&bot.env_json).unwrap_or_else(|_| serde_json::json!({}));
    let home = if host == crate::config::LOCAL_HOST {
        crate::home::dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default()
    } else {
        app.hosts().get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?.home().await?
    };
    Ok(crate::lifecycle::setup::grok_home(&env, &home))
}

/// 透過行程探測取得 pane 中的 grok pids。
pub async fn pids_in_pane(app: &impl crate::hosts::HostsAccess, host: &str, pane: &str, herdr_session: Option<&str>) -> Vec<i32> {
    match crate::memproc::dump(app, host).await {
        Ok(out) => crate::memproc::pids_in_pane(&out, pane, herdr_session),
        Err(e) => {
            tracing::debug!(error = ?e, "grok transcript: process dump failed; matching the session by cwd");
            Vec::new()
        }
    }
}

/// 載入 agy session。
pub async fn agy_session_load(
    app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::hosts::HostsAccess),
    run: &db::Run,
    host: &str,
) -> Result<Option<(String, String)>> {
    crate::lifecycle::agy_session::load(app, run, host).await
}

/// 記錄 agy status。
pub async fn agy_session_record_status(app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit), bot: &db::Bot, run: &db::Run, text: &str) {
    crate::lifecycle::agy_session::record_status(app, bot, run, text).await;
}

/// 處理 agy resume native 對話比對與 consume。
pub async fn consume_resume_session(
    app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands),
    bot: &db::Bot,
    run: &db::Run,
    session_id: Option<&str>,
) -> Result<()> {
    crate::hookrecv::consume_resume_session(app, bot, run, session_id).await
}

// --- Testing Helpers ---

#[cfg(test)]
pub async fn try_limit_hit_for_bot(app: &Arc<App>, bot: &db::Bot) -> Result<Option<crate::quota::LimitHit>> {
    crate::quota::try_limit_hit_for_bot(app, bot).await
}

#[cfg(test)]
pub async fn set_quota(app: &Arc<App>, host: &str, base: &str, quota: crate::quota::Quota) {
    crate::quota::set(app, host, base, quota).await;
}

#[cfg(test)]
pub async fn handle_status(app: &Arc<App>, host: &str, session: &str, event: &crate::herdr::Event) {
    crate::events::handle_status(app, host, session, event).await;
}
