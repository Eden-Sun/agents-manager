#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    // parent-uses
    use crate::lifecycle::*;
    use crate::db;
    use crate::lifecycle::fence::*;
    /// issue #98：`classify` 找上一代 run 要看 rowid（寫入順序），不能看 ULID 字典序——這裡故意讓
    /// **先寫進去**的那個 run 的 ULID 字典序反而比較大（模擬同一毫秒內隨機段沒有照時間排的情況），
    /// 證明圍籬還是正確認得出它是舊世代，沒有被字典序騙過去。
    #[tokio::test]
    async fn classify_finds_the_prior_run_by_insertion_order_even_when_its_ulid_sorts_after_the_current_one() {
        use crate::testing as tt;
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'fence-rowid','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        // 故意反過來：先寫進去的（真正的上一代）用一個字典序**比較大**的假 ULID。
        let old_run_id = "01ZZZZZZZZZZZZZZZZZZZZZZZZ";
        let new_run_id = "01AAAAAAAAAAAAAAAAAAAAAAAA";
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, native_session_id, started_at, ended_at)
             VALUES (?,?,'exited','unknown','pane-old','s-old',?,?)",
        )
        .bind(old_run_id)
        .bind(&bot_id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, started_at)
             VALUES (?,?,'running','working','pane-new',?)",
        )
        .bind(new_run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let new_run = sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id = ?").bind(new_run_id).fetch_one(&app.db).await.unwrap();

        let ev = EventIdentity { run_id: None, session_id: Some("s-old") };
        let out = classify(&app.db, &bot_id, &new_run, ev).await;
        assert!(
            matches!(&out, Ownership::Stale { prior_run_id, .. } if prior_run_id == old_run_id),
            "寫入順序證得出 old_run 是上一代，不該被 ULID 字典序騙成 Unproven：{out:?}"
        );
    }
}
