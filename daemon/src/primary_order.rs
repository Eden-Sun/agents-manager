//! 主力那列的固定順序（issue #344）：`bots.primary_position`，純顯示、只存 DB、不進 config.toml（同 `is_primary`：
//! 手機與桌機追的是同一組）。1 起算，`0`＝從沒排過。
//!
//! - 拖曳重排：`POST /api/order {"primary": [bot_id…]}` 把陣列位置（1 起算）寫進去，沒點名的維持原值。
//! - 新釘選的排到最後（全表 `max + 1`）；取消釘選**不清**位置，再釘回來還在原位（位置已經有值就不動）。

use crate::lifecycle::LcError;
use sqlx::SqlitePool;

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// `PATCH /api/bots/{id}` 的 `primary`：回受影響的列數（0＝沒有這顆 bot）。
pub async fn set_pinned(db: &SqlitePool, bot_id: &str, pin: bool) -> Result<u64, sqlx::Error> {
    // 只在第一次釘（位置還是 0）時排到最後；`max` 含已取消釘選的，所以新釘的一定在所有舊位置之後。
    let sql = if pin {
        "UPDATE bots SET is_primary = 1,
           primary_position = CASE WHEN primary_position = 0
             THEN (SELECT COALESCE(MAX(primary_position), 0) + 1 FROM bots) ELSE primary_position END
         WHERE id = ? AND deleted_at IS NULL"
    } else {
        "UPDATE bots SET is_primary = 0 WHERE id = ? AND deleted_at IS NULL"
    };
    Ok(sqlx::query(sql).bind(bot_id).execute(db).await?.rows_affected())
}

/// 陣列有重複、或點名了不存在（含已刪除）的 bot 都回 400，一筆都不寫。
pub async fn validate(db: &SqlitePool, ids: &[String]) -> Result<(), LcError> {
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if !seen.insert(id.as_str()) {
            return Err(LcError::Bad(format!("order: primary 裡 `{id}` 重複")));
        }
    }
    for id in ids {
        let live: Option<i64> = sqlx::query_scalar("SELECT 1 FROM bots WHERE id = ? AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(db)
            .await
            .map_err(up)?;
        if live.is_none() {
            return Err(LcError::Bad(format!("order: 未知的 bot `{id}`")));
        }
    }
    Ok(())
}

/// 陣列位置（1 起算）寫進 `primary_position`，同一個交易。呼叫前先 [`validate`]。
pub async fn write(db: &SqlitePool, ids: &[String]) -> Result<(), LcError> {
    let mut tx = db.begin().await.map_err(up)?;
    for (i, id) in ids.iter().enumerate() {
        sqlx::query("UPDATE bots SET primary_position = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(i as i64 + 1)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(up)?;
    }
    tx.commit().await.map_err(up)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    async fn pos(db: &SqlitePool, id: &str) -> (i64, i64) {
        sqlx::query_as("SELECT is_primary, primary_position FROM bots WHERE id = ?").bind(id).fetch_one(db).await.unwrap()
    }

    async fn three() -> (testing::Env, [String; 3]) {
        let e = testing::env().await;
        let a = testing::claude_bot(&e.app, &e.project_id, "a").await.id;
        let b = testing::claude_bot(&e.app, &e.project_id, "b").await.id;
        let c = testing::claude_bot(&e.app, &e.project_id, "c").await.id;
        (e, [a, b, c])
    }

    #[tokio::test]
    async fn newly_pinned_bots_go_last_in_pin_order() {
        let (e, [a, b, c]) = three().await;
        assert_eq!(pos(&e.app.db, &a).await, (0, 0), "沒釘過＝位置 0");
        for id in [&b, &c, &a] {
            assert_eq!(set_pinned(&e.app.db, id, true).await.unwrap(), 1);
        }
        assert_eq!(pos(&e.app.db, &b).await, (1, 1));
        assert_eq!(pos(&e.app.db, &c).await, (1, 2));
        assert_eq!(pos(&e.app.db, &a).await, (1, 3));
        // 重複釘（冪等）不動位置。
        set_pinned(&e.app.db, &b, true).await.unwrap();
        assert_eq!(pos(&e.app.db, &b).await, (1, 1));
    }

    #[tokio::test]
    async fn unpinning_keeps_the_position_and_repinning_restores_it() {
        let (e, [a, b, _]) = three().await;
        set_pinned(&e.app.db, &a, true).await.unwrap();
        set_pinned(&e.app.db, &b, true).await.unwrap();
        set_pinned(&e.app.db, &a, false).await.unwrap();
        assert_eq!(pos(&e.app.db, &a).await, (0, 1), "取消釘選不清位置");
        // 別顆新釘的排在所有舊位置之後（含已取消的）。
        let c = testing::claude_bot(&e.app, &e.project_id, "d").await.id;
        set_pinned(&e.app.db, &c, true).await.unwrap();
        assert_eq!(pos(&e.app.db, &c).await, (1, 3));
        set_pinned(&e.app.db, &a, true).await.unwrap();
        assert_eq!(pos(&e.app.db, &a).await, (1, 1), "再釘回來位置還在");
    }

    #[tokio::test]
    async fn pinning_a_missing_or_deleted_bot_touches_nothing() {
        let (e, [a, ..]) = three().await;
        assert_eq!(set_pinned(&e.app.db, "nope", true).await.unwrap(), 0);
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db_now()).bind(&a).execute(&e.app.db).await.unwrap();
        assert_eq!(set_pinned(&e.app.db, &a, true).await.unwrap(), 0);
    }

    fn db_now() -> String {
        crate::db::now()
    }

    #[tokio::test]
    async fn writing_an_order_sets_positions_and_leaves_the_unnamed_alone() {
        let (e, [a, b, c]) = three().await;
        for id in [&a, &b, &c] {
            set_pinned(&e.app.db, id, true).await.unwrap();
        }
        // 拖成 c, a；b 沒點名。
        write(&e.app.db, &[c.clone(), a.clone()]).await.unwrap();
        assert_eq!(pos(&e.app.db, &c).await.1, 1);
        assert_eq!(pos(&e.app.db, &a).await.1, 2);
        assert_eq!(pos(&e.app.db, &b).await.1, 2, "沒點名的維持原值");
    }

    #[tokio::test]
    async fn an_order_with_an_unknown_deleted_or_repeated_bot_is_refused() {
        let (e, [a, b, _]) = three().await;
        for bad in [vec![a.clone(), "ghost".to_string()], vec![a.clone(), a.clone()]] {
            let LcError::Bad(_) = validate(&e.app.db, &bad).await.unwrap_err() else { panic!() };
        }
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db_now()).bind(&b).execute(&e.app.db).await.unwrap();
        assert!(validate(&e.app.db, &[a.clone(), b]).await.is_err());
        assert!(validate(&e.app.db, &[a]).await.is_ok());
    }
}
