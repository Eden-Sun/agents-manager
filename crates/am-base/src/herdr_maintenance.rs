//! 計畫中的 herdr server 重啟（SPEC §6.5.2，AGM 2026-09-17 herdr 0.9.0 升級）。
//!
//! 平常 reconcile 看到子 agent 的 agent 不在，就把那顆子 bot 軟刪：子 agent 只活在父開的 pane 裡。
//! 但 herdr server 一重啟，**所有** pane 同時消失——那不是「子 agent 做完了」，照平常的規則會把每一顆
//! 子 agent 一次刪光，事後接回原對話也叫不回來。
//!
//! 所以要有一個**明確、有時限、有稽核**的維護狀態：只有 AGM 角色開得了、上限 30 分鐘、逾時自動結束，
//! 開／關／逾時都寫 `supervisor_notes`。期間 reconcile 照樣把 run 標成 exited，只是不刪子 bot；
//! 維護結束（或逾時）時，這段期間被標 exited、到現在仍沒接回的子 agent 才照原規則退休。

use anyhow::Result;
use serde::Deserialize;
use sqlx::SqlitePool;

/// 維護窗口的上限（分鐘）。升級實測一輪不到 5 分鐘；30 分鐘給回滾留空間，又不會讓「忘了關」變成常態。
pub const MAX_MINUTES: i64 = 30;

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS herdr_maintenance (
           id INTEGER PRIMARY KEY CHECK (id = 1),
           opened_at TEXT NOT NULL,
           until TEXT NOT NULL,
           opened_by TEXT NOT NULL,
           reason TEXT
         )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct Window {
    pub opened_at: String,
    pub until: String,
    pub opened_by: String,
    pub reason: Option<String>,
}

pub async fn row(pool: &SqlitePool) -> Result<Option<Window>> {
    Ok(sqlx::query_as::<_, Window>("SELECT opened_at, until, opened_by, reason FROM herdr_maintenance WHERE id = 1")
        .fetch_optional(pool)
        .await?)
}

#[derive(Deserialize, Default)]
pub struct OpenIn {
    pub minutes: Option<i64>,
    pub reason: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct CloseIn {
    pub reason: Option<String>,
}
