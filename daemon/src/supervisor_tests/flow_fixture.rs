//! #1862 S3b：daemon 版的 supervisor 測試夾具（原本是 `am-supervisor/src/supervisor/bot_requests.rs` 的 `flow_tests` 在 harness 下的分支）。
//! 真 `App`（`app_ports_p1::open`＋`load_config`＋不存在的 herdr socket）與 patrol／responder／兩顆 worker bot 的種子資料；
//! 函式本體與原夾具逐字相同。使用者：`supervisor_tests::{controller,health,responder,incidents,bot_requests}`、
//! `api::relay_announce_tests`、`api::relay_from_auth_tests`、`runners::supervisor::{responder_api,api}` 的測試。

use crate::state::App;
use crate::supervisor::roles::{self, Role};
use crate::supervisor::store;
use std::sync::Arc;

pub(crate) async fn app() -> Arc<App> {
    let dir = crate::testing::scratch_dir("agm-bot-requests");
    let db = crate::app_ports_p1::open(&dir.join("test.sqlite")).await.unwrap();
    let cfg = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
    let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
    let app = App::new(db, client.clone(), client, cfg, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false);
    store::get_or_init(&app.db).await.unwrap();
    sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(crate::db::now()).execute(&app.db).await.unwrap();
    for (id, name) in [("patrol", "AGM"), ("resp", "AGM-responder"), ("w1", "fixer"), ("w2", "builder")] {
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,identity,hook_token,created_at) VALUES (?,'p',?,'claude','cc0',?,?)")
            .bind(id)
            .bind(name)
            .bind(format!("tok-{id}"))
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
    }
    store::set_env(&app.db, "patrol", "p", "/tmp").await.unwrap();
    app
}

pub(crate) async fn configure_responder(app: &Arc<App>) {
    roles::set_env(&app.db, Role::Responder, "resp", "p", "/tmp").await.unwrap();
}
