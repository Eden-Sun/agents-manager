//! P10（分享／outbox）的 App 端接縫：把 `outbox`、`outbox_remote`、`local_image`、`share::{cage,svg_check,portal}` 要的窄介面用 `App` 實作出來，
//! 並提供路由表用的 axum handler（`State<Arc<App>>`）。`App` 只活在這個檔（composition 層）；授權／安全規則全留在原模組，不經過這裡改寫。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::response::Response;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::lifecycle::LcError;
use crate::outbox::{BotLookup, BotPlace, OutboxEnv, ShareStorage};
use crate::share::portal::{PortalEnv, SendOutcome};
use crate::share::svg_check::SvgCheckEnv;
use crate::state::App;

impl ShareStorage for App {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn db_pool(&self) -> &sqlx::SqlitePool {
        &self.db
    }
}

impl OutboxEnv for App {
    fn bot_place(&self, bot_id: &str) -> impl Future<Output = Result<BotPlace, BotLookup>> + Send {
        let bot_id = bot_id.to_string();
        async move {
            let bot = match crate::db::bot(&self.db, &bot_id).await {
                Ok(Some(bot)) => bot,
                Ok(None) => return Err(BotLookup::BotMissing),
                Err(_) => return Err(BotLookup::BotUnavailable),
            };
            let project = match crate::db::project(&self.db, &bot.project_id).await {
                Ok(Some(project)) => project,
                Ok(None) => return Err(BotLookup::ProjectMissing),
                Err(_) => return Err(BotLookup::ProjectUnavailable),
            };
            Ok(BotPlace { host: project.host, project_path: project.path, cwd: bot.cwd })
        }
    }
}

impl crate::outbox_remote::OutboxRemoteEnv for App {
    fn host_conn(&self, host: &str) -> impl Future<Output = Option<Arc<crate::hosts::HostConn>>> + Send {
        let host = host.to_string();
        async move { self.hosts.get(&host).await }
    }

    fn instance(&self) -> Option<String> {
        App::instance(self)
    }
}

impl crate::share::cage::CageEnv for App {
    fn local_identity_env(app: &Arc<Self>, identity: &str) -> impl Future<Output = std::collections::BTreeMap<String, String>> + Send {
        let app = app.clone();
        let identity = identity.to_string();
        async move {
            crate::tools::identity_for_host(&app, crate::config::LOCAL_HOST, &identity).await.map(|i| i.env).unwrap_or_default()
        }
    }

    fn bot_dir(&self, bot_id: &str) -> anyhow::Result<PathBuf> {
        App::bot_dir(self, bot_id)
    }

    fn claude_models(app: &Arc<Self>, identity: Option<&str>) -> impl Future<Output = Result<Value, String>> + Send {
        let app = app.clone();
        let identity = identity.map(str::to_string);
        async move {
            crate::runners::models::list(&app, crate::config::LOCAL_HOST, "claude", identity.as_deref(), false).await.map_err(|e| e.to_string())
        }
    }
}

impl SvgCheckEnv for App {
    fn remind_bot(app: &Arc<Self>, bot_id: &str, text: &str, crid: &str) -> impl Future<Output = Result<(), String>> + Send {
        let app = app.clone();
        let (bot_id, text, crid) = (bot_id.to_string(), text.to_string(), crid.to_string());
        async move {
            let src = crate::lifecycle::RelaySrc { from: Some(crate::agent_relay::DAEMON_SENDER), unverified: false };
            crate::lifecycle::start_send::prompt_starting_or_queue(&app, &bot_id, &text, &crid, &[], src, true)
                .await
                .map(|_| ())
                .map_err(|e| format!("{e:?}"))
        }
    }
}

impl PortalEnv for App {
    fn share_base_url(&self) -> impl Future<Output = Option<String>> + Send {
        async move { self.cfg.get().await.share.base() }
    }

    fn share_listen(&self) -> impl Future<Output = Option<String>> + Send {
        async move { self.cfg.get().await.share.listen.clone() }
    }

    fn bot_mutex(&self, bot_id: &str) -> impl Future<Output = Arc<tokio::sync::Mutex<()>>> + Send {
        let bot_id = bot_id.to_string();
        async move { self.bot_lock(&bot_id).await }
    }

    fn share_status(&self, bot_id: &str) -> impl Future<Output = &'static str> + Send {
        let bot_id = bot_id.to_string();
        async move {
            match crate::db::active_run(&self.db, &bot_id).await {
                Ok(run) => crate::api::lamp(self.bot_connected(&bot_id).await, run.as_ref()),
                Err(_) => "unknown",
            }
        }
    }

    fn subscribe_events(&self) -> broadcast::Receiver<crate::state::WsEvent> {
        self.subscribe()
    }

    fn send_share_message(app: &Arc<Self>, bot_id: &str, composed: &str, crid: &str, token: &str) -> impl Future<Output = SendOutcome> + Send {
        let app = app.clone();
        let (bot_id, composed, crid, token) = (bot_id.to_string(), composed.to_string(), crid.to_string(), token.to_string());
        async move {
            let src = crate::lifecycle::RelaySrc::trusted(Some(crate::share::SHARE_SENDER));
            match crate::lifecycle::prompt_starting_or_queue_with_share_token(&app, &bot_id, &composed, &crid, &[], src, true, &token).await {
                Ok(out) => SendOutcome::Accepted { message_id: out.message_id, delivery: out.delivery },
                Err(LcError::NotFound(_)) => SendOutcome::NotFound,
                Err(LcError::Conflict(v)) => SendOutcome::Conflict(v),
                Err(e) => SendOutcome::Failed(format!("{e:?}")),
            }
        }
    }
}

// ───────── 路由用的 axum handler（舊名由原模組 re-export） ─────────

/// `GET /api/bots/{id}/outbox`
pub async fn list(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Result<Response, LcError> {
    crate::outbox::list_for(&app, id).await
}

/// `GET /api/bots/{id}/outbox/file?path=…`
pub async fn file(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, LcError> {
    crate::outbox::file_for(&app, id, q).await
}

/// `GET /api/bots/{id}/local-image?path=…`
pub async fn get(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Response, LcError> {
    crate::local_image::get_for(&app, id, q).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::ShareFileError;
    use crate::testing::{claude_bot, env};

    /// bot／專案兩步的查詢結果要分得出來：原本各處對外的講法不同（本機圖片：bot 不在＝bot、專案不在＝image）。
    #[tokio::test]
    async fn the_bot_place_lookup_tells_a_missing_bot_from_a_missing_project() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "place").await;
        let place = e.app.bot_place(&bot.id).await.unwrap();
        assert_eq!(place.host, crate::config::LOCAL_HOST);
        assert_eq!(e.app.bot_place("no-such-bot").await.unwrap_err(), BotLookup::BotMissing);
        // 外鍵不讓 bot 指到不存在的專案：這一條連線暫時關掉外鍵檢查，造出「bot 在、專案不在」。
        let mut conn = e.app.db.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut *conn).await.unwrap();
        sqlx::query("UPDATE bots SET project_id = 'ghost-project' WHERE id = ?").bind(&bot.id).execute(&mut *conn).await.unwrap();
        sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await.unwrap();
        drop(conn);
        assert_eq!(e.app.bot_place(&bot.id).await.unwrap_err(), BotLookup::ProjectMissing);
    }

    #[tokio::test]
    async fn the_share_download_and_the_local_image_keep_their_own_not_found_wording() {
        let e = env().await;
        let missing = crate::outbox::share_file_bytes(&e.app, "no-such-bot", "a.txt").await;
        assert!(matches!(missing, Err(ShareFileError::NotFound)));
        match crate::local_image::get_for(&e.app, "no-such-bot".into(), [("path".to_string(), "a.png".to_string())].into()).await {
            Err(LcError::NotFound(what)) => assert_eq!(what, "bot"),
            other => panic!("{other:?}"),
        }
        match crate::local_image::get_for(&e.app, "no-such-bot".into(), Default::default()).await {
            Err(LcError::Bad(_)) => {}
            other => panic!("沒帶 path 要先 400：{other:?}"),
        }
    }
}
