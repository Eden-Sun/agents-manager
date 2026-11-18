#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::events::test_seams::*;
    use crate::events::test_seams::apply_agent_title;
    use crate::lifecycle_tests::prelude::*;
    use crate::events::*;
    async fn title_of(db: &sqlx::SqlitePool, run: &str) -> Option<String> {
        sqlx::query_scalar("SELECT agent_title FROM runs WHERE id = ?").bind(run).fetch_one(db).await.unwrap()
    }
    /// 名稱不當參照：標題寫回靠 host＋session＋pane_id 找 run。run 上的 agent_name 是會改的顯示字串——
    /// 改名後仍要寫得進去；別台 host 上同 pane id、同 agent 名的 run、以及別人用了舊名，都不能被寫到。
    #[tokio::test]
    async fn title_write_back_finds_the_run_by_pane_not_by_name() {
        use crate::testing as tt;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "title-bot").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        // 使用者把 herdr agent 改名：DB 裡的 agent_name 跟 herdr 現在報的名字對不上。
        sqlx::query("UPDATE runs SET agent_name = 'renamed-away' WHERE id = ?").bind(&run).execute(&app.db).await.unwrap();

        // 別台 host 的 run：同 pane id、同 session、agent_name 恰好是 herdr 現在報的名字。
        let remote_project = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, 'remote', 'mac2', ?)")
            .bind(&remote_project)
            .bind(format!("{}/remote", e.dir.display()))
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let remote_bot = tt::claude_bot(&app, &remote_project, "title-remote").await;
        let remote_run = tt::fake_run(&app, &remote_bot.id).await;
        sqlx::query("UPDATE runs SET pane_id = ?, agent_name = 'agent' WHERE id = ?").bind(&pane).bind(&remote_run).execute(&app.db).await.unwrap();

        apply_agent_title(&*app, "local", "test", "test", &pane, "修 bug").await;
        assert_eq!(title_of(&app.db, &run).await.as_deref(), Some("修 bug"), "改名後仍以 pane 認得 run");
        assert_eq!(title_of(&app.db, &remote_run).await, None, "別台 host 同 pane id 的 run 不會被寫到");

        apply_agent_title(&*app, "mac2", "test", "test", &pane, "遠端標題").await;
        assert_eq!(title_of(&app.db, &remote_run).await.as_deref(), Some("遠端標題"));
        assert_eq!(title_of(&app.db, &run).await.as_deref(), Some("修 bug"), "本機的 run 不受遠端那次影響");

        apply_agent_title(&*app, "local", "test", "test", "pane-unknown", "不存在").await;
        assert_eq!(title_of(&app.db, &run).await.as_deref(), Some("修 bug"), "pane 對不上就不寫，不改用名稱猜");
    }
}
