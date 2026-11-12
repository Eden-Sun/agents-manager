use super::*;

#[tokio::test]
async fn the_health_snapshot_does_no_per_bot_lookups() {
    let env = crate::testing::env().await;
    for i in 0..15 {
        let b = crate::testing::claude_bot(&env.app, &env.project_id, &format!("health{i}")).await;
        if i % 3 != 0 {
            crate::testing::fake_run(&env.app, &b.id).await;
        }
    }
    db::PER_BOT_LOOKUPS.with(|c| c.set(0));
    let health = crate::supervisor::health::snapshot(&env.app).await.unwrap();
    assert_eq!(db::PER_BOT_LOOKUPS.with(|c| c.get()), 0, "每顆 bot 再查一次 active_run＝N+1，要改成一次讀完");
    assert_eq!(health["bots"]["total"], 15);
    assert_eq!(health["bots"]["running"], 10);
    assert_eq!(health["bots"]["stopped"], 5);
}
