//! ops 腳本直接對 daemon 的 DB 下的 SQL，要能在真 schema 上編譯（#1458，#1391 同型的守門）。
//! 抽不到 SQL 就 panic：腳本改寫法時這支要紅，不能靜靜變成 0 句。
//!
//! 腳本在測試執行時從 repo 讀，不用 `include_str!`：契約測試只需要在測試裡拿到 SQL，
//! 不該把腳本編進正式二進位（那會讓 `persona` 的 BUILD_INPUTS 守門要求把 scripts/ops 也列進重建條件）。

use crate::{db, testing as tt};
use sqlx::Row;

/// repo 內的腳本（相對於 repo 根）；`CARGO_MANIFEST_DIR` 是 `daemon/`，它的上一層就是 repo 根。
fn read_script(rel: &str) -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("讀不到 {rel}：{e}"))
}

/// `outbox-gc.sh` 的分享名單查詢：唯一一行以 `SHARE_SQL="` 開頭，取兩個雙引號之間的字。
fn share_sql(script: &str) -> String {
    let line = script
        .lines()
        .find(|l| l.starts_with("SHARE_SQL=\""))
        .expect("outbox-gc.sh 沒有 SHARE_SQL=\"…\" 這一行（改寫法了？）");
    let body = &line["SHARE_SQL=\"".len()..];
    let end = body.find('"').expect("SHARE_SQL 沒有收尾的雙引號");
    body[..end].to_string()
}

/// `herdr-upgrade.sh` 裡 `read_running_snapshot` 與升級後比對的兩句：從 `"SELECT b.id, b.name` 到 `ORDER BY b.name"`。
fn herdr_sqls(script: &str) -> Vec<String> {
    const START: &str = "\"SELECT b.id, b.name";
    const END: &str = "ORDER BY b.name";
    let mut out = Vec::new();
    let mut rest = script;
    while let Some(start) = rest.find(START) {
        let after = &rest[start + 1..];
        let end = after.find(&format!("{END}\"")).expect("herdr-upgrade.sh 的 SQL 沒有以 ORDER BY b.name\" 結束");
        out.push(after[..end + END.len()].to_string());
        rest = &after[end + END.len() + 1..];
    }
    assert_eq!(out.len(), 2, "herdr-upgrade.sh 應該有 2 句 SQL（快照與升級後比對），實際 {}", out.len());
    out
}

#[tokio::test]
async fn every_ops_sql_statement_compiles_against_the_real_schema() {
    let e = tt::env().await;
    let gc = read_script("scripts/ops/outbox-gc.sh");
    let upgrade = read_script("scripts/ops/herdr-upgrade.sh");
    let mut stmts = vec![("outbox-gc.sh", share_sql(&gc))];
    stmts.extend(herdr_sqls(&upgrade).into_iter().map(|s| ("herdr-upgrade.sh", s)));
    assert_eq!(stmts.len(), 3, "抽出來的句數要是 1（outbox-gc）+ 2（herdr-upgrade）");
    for (script, sql) in stmts {
        if let Err(err) = sqlx::query(&format!("EXPLAIN {sql}")).fetch_all(&e.app.db).await {
            panic!("{script} 的 SQL 在真 schema 上編不過：{err}\n{sql}");
        }
    }
}

#[tokio::test]
async fn the_share_list_query_returns_only_live_shared_bots() {
    let e = tt::env().await;
    let app = e.app.clone();
    let live = tt::claude_bot(&app, &e.project_id, "ops-share-live").await;
    let gone = tt::claude_bot(&app, &e.project_id, "ops-share-gone").await;
    for bot in [&live, &gone] {
        crate::share::store::insert_share_bot(&app.db, &bot.id, crate::share::store::PROFILE_TRUSTED, "/tmp/ops-sql-contract")
            .await
            .unwrap();
    }
    sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?")
        .bind(db::now())
        .bind(&gone.id)
        .execute(&app.db)
        .await
        .unwrap();
    let sql = share_sql(&read_script("scripts/ops/outbox-gc.sh"));
    let ids: Vec<String> = sqlx::query_scalar::<_, String>(&sql).fetch_all(&app.db).await.unwrap();
    assert_eq!(ids, vec![live.id.clone()], "只有沒刪的分享 bot 在名單上");
}

#[tokio::test]
async fn the_running_snapshot_marks_a_bot_with_an_in_flight_turn() {
    let e = tt::env().await;
    let app = e.app.clone();
    let bot = tt::claude_bot(&app, &e.project_id, "ops-snap-busy").await;
    let run_id = tt::fake_run(&app, &bot.id).await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at) VALUES ('t-ops-snap',?,?,'web','in_flight','ok',?)")
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let sql = herdr_sqls(&read_script("scripts/ops/herdr-upgrade.sh")).remove(0);
    let rows = sqlx::query(&sql).fetch_all(&app.db).await.unwrap();
    let row = rows
        .iter()
        .find(|r| r.try_get::<String, _>(0).ok().as_deref() == Some(bot.id.as_str()))
        .expect("快照要列出這顆 running 的 bot");
    let state: String = row.try_get(4).unwrap();
    let in_flight: i64 = row.try_get(8).unwrap();
    assert_eq!((state.as_str(), in_flight), ("running", 1), "有 in_flight 的回合：第 9 欄是 1");
}
