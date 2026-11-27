//! 回合收尾的副作用：hook 收尾（`hookrecv`）與 poller 備援收尾（`try_fallback`、`capture_hookless_turn_locked`）共用同一份，
//! 不論回合是誰收的都做到（codex 的 Stop hook 沒被信任時，每一回合都走備援）。

use crate::db;
use crate::hosts::HostFence;
use std::future::Future;

/// 收尾副作用需要的宿主動作。future 一律 `Send`：poller 的備援在 `tokio::spawn` 裡收尾。
pub trait TurnCloseServices: Send + Sync {
    /// bot 所在主機現在的世代（備援收尾沒有 hook 放行時捕獲的那一代，就用當下的）。
    fn turn_close_fence(&self, bot: &db::Bot) -> impl Future<Output = Option<HostFence>> + Send;
    fn turn_close_auth_failure(&self, bot: &db::Bot, fence: Option<&HostFence>) -> impl Future<Output = ()> + Send;
    fn turn_close_ok(&self, bot: &db::Bot, fence: Option<&HostFence>) -> impl Future<Output = ()> + Send;
    fn turn_close_clear_limit(&self, bot: &db::Bot) -> impl Future<Output = ()> + Send;
}

impl<T: TurnCloseServices + ?Sized> TurnCloseServices for std::sync::Arc<T> {
    fn turn_close_fence(&self, bot: &db::Bot) -> impl Future<Output = Option<HostFence>> + Send { T::turn_close_fence(self.as_ref(), bot) }
    fn turn_close_auth_failure(&self, bot: &db::Bot, fence: Option<&HostFence>) -> impl Future<Output = ()> + Send { T::turn_close_auth_failure(self.as_ref(), bot, fence) }
    fn turn_close_ok(&self, bot: &db::Bot, fence: Option<&HostFence>) -> impl Future<Output = ()> + Send { T::turn_close_ok(self.as_ref(), bot, fence) }
    fn turn_close_clear_limit(&self, bot: &db::Bot) -> impl Future<Output = ()> + Send { T::turn_close_clear_limit(self.as_ref(), bot) }
}

/// 收掉這個 run 的「壓縮中」標記（`PreCompact` 開的與畫面記的一起），有收掉就推 `bot_status`。
pub async fn end_compacting(app: &impl crate::capabilities::BotStatusEmit, run_id: &str, bot_id: &str) {
    if am_base::compacting::end(run_id) {
        app.emit_bot_status(bot_id).await;
    }
}

/// claude／codex 答完一回合：回覆是授權失敗那一句就標「這個身份要登入」並推播；其他非空回覆就清掉標記。
/// `fence`：這一則放行時（或備援收尾當下）的主機世代。
pub async fn login_after_reply(app: &impl TurnCloseServices, kind: &str, bot: &db::Bot, reply: &str, fence: Option<&HostFence>) {
    if !matches!(kind, "claude" | "codex") {
        return;
    }
    if crate::login_prompt::is_auth_failure_line(kind, reply) {
        app.turn_close_auth_failure(bot, fence).await;
    } else if !reply.trim().is_empty() {
        app.turn_close_ok(bot, fence).await;
    }
}

/// codex 有了非空的原生回覆（hook 或 rollout）＝帳號又能跑了：清掉這顆 bot 的撞限。
pub async fn clear_codex_limit_after_reply(app: &impl TurnCloseServices, kind: &str, bot: &db::Bot, reply: &str) {
    if kind == "codex" && !reply.trim().is_empty() {
        app.turn_close_clear_limit(bot).await;
    }
}

/// 這句 prompt 回音是別的 agent 打進來的嗎？（見 SPEC §6.5d）認出來就用掉那一筆登記。
pub async fn relay_source<'e>(conn: impl sqlx::Executor<'e, Database = sqlx::Sqlite>, run: Option<&db::Run>, echo: &str) -> Option<crate::agent_relay::Relayed> {
    let run = run?;
    let host: Option<String> = sqlx::query_scalar("SELECT p.host FROM bots b JOIN projects p ON p.id=b.project_id WHERE b.id=?")
        .bind(&run.bot_id).fetch_optional(conn).await.ok()?;
    let host = host.unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    crate::agent_relay::claim_relayed(&host, &run.id, echo)
}

/// 外部回合的使用者訊息：寫入時就帶上 `relay_from`／`relay_turn_id`（別的 bot 打進來的字），hook 與備援都走這支。
pub async fn insert_user_echo_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run: Option<&db::Run>,
    conversation_id: &str,
    turn_id: &str,
    text: &str,
    source: &str,
) -> anyhow::Result<db::Message> {
    let from = relay_source(&mut **tx, run, text).await;
    let msg = crate::lifecycle::messages::insert_message_relayed_tx(
        tx, conversation_id, Some(turn_id), "user", text, source, false, None, from.as_ref().map(|r| r.from_bot.as_str()),
    )
    .await?;
    crate::lifecycle::messages::mark_relay_turn(tx, &msg.id, from.as_ref().and_then(|r| r.from_turn.as_deref())).await?;
    Ok(msg)
}
