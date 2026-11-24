//! Refresh a late native session binding for a fenced Codex run.

use super::super::*;

/// Best-effort repair only for a freshly observed, fenced Codex instance.
pub(super) async fn refresh(app: &impl LcHost, run: &db::Run, bot: &db::Bot, turn: &str) -> Option<db::Run> {
    if bot.kind != "codex" || run.native_session_id.is_some() {
        return None;
    }
    let result = refresh_inner(app, run, Some(turn)).await;
    let reason = match &result { Ok(Some(_)) => "bound", Ok(None) => "not_proven", Err(_) => "read_or_write_failed" };
    let basis = if matches!(&result, Ok(Some(_))) { "same_host_session_pane_terminal" } else { "not_proven" };
    tracing::info!(run = %run.id, turn, adopted = run.adopted, reason, basis, "codex native session refresh");
    result.ok().flatten()
}

/// 兩次補綁嘗試之間至少隔這麼久（每 run）；成功、沒證據、失敗都算一次，遠端主機不會被每輪巡邏打 ssh。
const IDLE_BIND_EVERY: Duration = Duration::from_secs(30);

fn idle_bind_due(run_id: &str, now: std::time::Instant) -> bool {
    static LAST: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> = std::sync::OnceLock::new();
    let mut m = LAST.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if m.get(run_id).is_some_and(|at| now.duration_since(*at) < IDLE_BIND_EVERY) {
        return false;
    }
    if m.len() > 512 {
        m.retain(|_, at| now.duration_since(*at) < IDLE_BIND_EVERY);
    }
    m.insert(run_id.to_string(), now);
    true
}

/// 沒有回合也補綁：run 的 `native_session_id` 還是 NULL 時問 herdr 這個 pane 的 `agent_session`（讀 rollout 的 context 用量要靠它）。
/// 保護規則與回合路徑完全相同（同主機 fence、pane＋terminal＋herdr_session＋host＋generation 都對得上才寫，沒有同 bot 的別的 active run）；
/// 只少了「有在途回合」這一條。比對不到就不綁。每 run 至少隔 [`IDLE_BIND_EVERY`] 才嘗試，單次查詢 5 秒逾時。
pub(super) async fn refresh_idle(app: &impl LcHost, run: &db::Run) -> Option<db::Run> {
    if run.native_session_id.is_some() || !matches!(run.state.as_str(), "starting" | "running") || !idle_bind_due(&run.id, std::time::Instant::now()) {
        return None;
    }
    let result = refresh_inner(app, run, None).await;
    let reason = match &result { Ok(Some(_)) => "bound", Ok(None) => "not_proven", Err(_) => "read_or_write_failed" };
    tracing::info!(run = %run.id, reason, "codex native session idle bind");
    result.ok().flatten()
}

async fn refresh_inner(app: &impl LcHost, run: &db::Run, turn: Option<&str>) -> anyhow::Result<Option<db::Run>> {
    let Some(pane) = run.pane_id.as_deref().filter(|s| !s.trim().is_empty()) else { return Ok(None) };
    let Some(session) = run.herdr_session.as_deref().filter(|s| !s.trim().is_empty()) else { return Ok(None) };
    let Some(terminal) = run.terminal_id.as_deref().filter(|s| !s.trim().is_empty()) else { return Ok(None) };
    let (host, generation): (Option<String>, Option<i64>) = sqlx::query_as("SELECT host,host_generation FROM runs WHERE id=?")
        .bind(&run.id).fetch_one(app.db()).await?;
    let client = match super::super::client_for_run(app, run).await { Ok(c) => c, Err(_) => return Ok(None) };
    if host.as_deref().is_some_and(|h| h != client.fence.conn().name)
        || generation != client.fence.target_epoch() { return Ok(None) }
    let started = std::time::Instant::now();
    let observed = match tokio::time::timeout(Duration::from_secs(5), client.agent_get(pane)).await {
        Ok(Ok(Some(agent))) => agent,
        _ => return Ok(None),
    };
    if observed.pane_id != pane || observed.agent.as_deref() != Some("codex")
        || observed.terminal_id.as_deref() != Some(terminal) { return Ok(None) }
    let Some(native) = super::super::dead_panes::herdr_session_id(&observed) else { return Ok(None) };
    #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
    super::super::race_point::hit("codex_session_refresh_before_write", &run.id).await;
    let result = client.fence.run_current(Box::pin(async {
        if started.elapsed() >= Duration::from_secs(5) { return Ok::<_, anyhow::Error>(None) }
        let changed = sqlx::query(
            "UPDATE runs SET native_session_id=? WHERE id=? AND bot_id=? AND adopted=?
             AND state IN ('starting','running','stopping') AND ended_at IS NULL AND native_session_id IS NULL
             AND pane_id=? AND herdr_session=? AND terminal_id=? AND host IS ? AND host_generation IS ?
             AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id
                        WHERE b.id=runs.bot_id AND b.kind='codex' AND b.deleted_at IS NULL AND p.host=?)
             AND (? IS NULL OR EXISTS(SELECT 1 FROM turns t WHERE t.id=? AND t.run_id=runs.id AND t.status='in_flight' AND t.delivery='ok'))
             AND NOT EXISTS(SELECT 1 FROM runs other WHERE other.bot_id=runs.bot_id AND other.id<>runs.id
                            AND other.state IN ('starting','running','stopping') AND other.ended_at IS NULL)"
        ).bind(&native).bind(&run.id).bind(&run.bot_id).bind(run.adopted).bind(pane).bind(session).bind(terminal)
            .bind(&host).bind(generation).bind(&client.fence.conn().name).bind(turn).bind(turn)
            .execute(app.db()).await?.rows_affected();
        if changed != 1 { return Ok(None) }
        db::run(app.db(), &run.id).await
    })).await;
    Ok(result.transpose()?.flatten())
}


#[cfg(any(test, feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
pub mod test_seams {
    use super::*;

    pub async fn refresh(app: &impl LcHost, run: &db::Run, bot: &db::Bot, turn: &str) -> Option<db::Run> {
        super::refresh(app, run, bot, turn).await
    }
    pub async fn refresh_idle(app: &impl LcHost, run: &db::Run) -> Option<db::Run> {
        super::refresh_idle(app, run).await
    }
}
