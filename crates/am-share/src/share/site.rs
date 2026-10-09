//! 分享 bot 檔案位置解析（SPEC §20、remote-share-design §3.3）。
//!
//! 把 `shared_bots.workspace` 與 `projects.host` 解析為這顆分享 bot 的實際檔案所在位置。
//! 本機為 `ShareSite::Local`，遠端主機為 `ShareSite::Remote`。
//! 任何不確定（DB 查詢錯誤、主機未連線、路徑不合法）一律 fail closed 回 `SiteError::Unavailable`。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use am_base::hosts::HostConn;
use sqlx::SqlitePool;

#[derive(Clone)]
pub enum ShareSite {
    Local {
        workspace: PathBuf,
        outbox: Option<PathBuf>,
    },
    Remote(RemoteSite),
}

impl std::fmt::Debug for ShareSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local { workspace, outbox } => f
                .debug_struct("Local")
                .field("workspace", workspace)
                .field("outbox", outbox)
                .finish(),
            Self::Remote(r) => f.debug_tuple("Remote").field(r).finish(),
        }
    }
}

#[derive(Clone)]
pub struct RemoteSite {
    pub conn: Arc<HostConn>,
    pub host: String,
    pub home: String,
    /// remote root 絕對路徑（例如 `~/.config/agents-manager[/instances/<slug>]` 解開後的完整路徑）
    pub root: String,
    pub workspace: String,
    pub outbox: String,
}

impl std::fmt::Debug for RemoteSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSite")
            .field("host", &self.host)
            .field("home", &self.home)
            .field("root", &self.root)
            .field("workspace", &self.workspace)
            .field("outbox", &self.outbox)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SiteError {
    #[error("not a share bot")]
    NotShareBot,
    #[error("share site unavailable")]
    Unavailable,
}

/// 解析分享位置所需的環境能力（由 composition 層實作）。
pub trait SiteEnv: Send + Sync {
    fn db_pool(&self) -> &SqlitePool;
    fn host_conn(&self, host: &str) -> impl Future<Output = Option<Arc<HostConn>>> + Send;
    fn instance(&self) -> Option<String>;
    fn data_dir(&self) -> &Path;
}

impl<T: SiteEnv + ?Sized> SiteEnv for Arc<T> {
    fn db_pool(&self) -> &SqlitePool {
        (**self).db_pool()
    }

    fn host_conn(&self, host: &str) -> impl Future<Output = Option<Arc<HostConn>>> + Send {
        (**self).host_conn(host)
    }

    fn instance(&self) -> Option<String> {
        (**self).instance()
    }

    fn data_dir(&self) -> &Path {
        (**self).data_dir()
    }
}

/// `shared_bots.workspace` ＋ `projects.host` → 這顆分享 bot 的檔案在哪。
/// DB 讀不到、主機不認得或斷線 → `Unavailable`。
pub async fn resolve(app: &impl SiteEnv, bot_id: &str) -> Result<ShareSite, SiteError> {
    resolve_inner(app, bot_id, false).await
}

/// 同 [`resolve`]，但軟刪除的 bot／專案也解析（收尾用：拿掉遠端 `.am-share-keep` 標記時 bot 或專案往往已經標成刪除了）。
pub async fn resolve_including_deleted(app: &impl SiteEnv, bot_id: &str) -> Result<ShareSite, SiteError> {
    resolve_inner(app, bot_id, true).await
}

async fn resolve_inner(app: &impl SiteEnv, bot_id: &str, include_deleted: bool) -> Result<ShareSite, SiteError> {
    let sql = if include_deleted {
        "SELECT r.workspace, p.host
         FROM shared_bots r
         JOIN bots b ON b.id = r.bot_id
         JOIN projects p ON p.id = b.project_id
         WHERE r.bot_id = ?"
    } else {
        "SELECT r.workspace, p.host
         FROM shared_bots r
         JOIN bots b ON b.id = r.bot_id AND b.deleted_at IS NULL
         JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL
         WHERE r.bot_id = ?"
    };
    let row: Option<(String, String)> = sqlx::query_as(sql)
    .bind(bot_id)
    .fetch_optional(app.db_pool())
    .await
    .map_err(|e| {
        tracing::warn!(bot_id, error = %e, "site::resolve DB query failed");
        SiteError::Unavailable
    })?;

    let (workspace, host) = match row {
        Some(pair) => pair,
        None => return Err(SiteError::NotShareBot),
    };

    if host == am_base::config::LOCAL_HOST {
        let ws_path = PathBuf::from(workspace);
        let outbox = am_base::outbox::dir_for(app.data_dir(), bot_id);
        Ok(ShareSite::Local {
            workspace: ws_path,
            outbox,
        })
    } else {
        let conn = app.host_conn(&host).await.ok_or(SiteError::Unavailable)?;
        if !conn.is_connected() {
            return Err(SiteError::Unavailable);
        }
        let home = conn.home().await.map_err(|e| {
            tracing::warn!(host = %host, error = %e, "site::resolve remote home failed");
            SiteError::Unavailable
        })?;
        let instance = app.instance();
        remote_site_at(conn, &host, home, instance.as_deref(), bot_id, workspace)
            .map(ShareSite::Remote)
            .ok_or(SiteError::Unavailable)
    }
}

/// 遠端分享 bot 的位置（root、outbox 的算法只在這裡）。建 bot 時資料還沒進 DB，所以 `resolve` 用不了，就用這個直接組。
pub fn remote_site_at(
    conn: Arc<HostConn>,
    host: &str,
    home: String,
    instance: Option<&str>,
    bot_id: &str,
    workspace: String,
) -> Option<RemoteSite> {
    let rel_root = am_base::hosts::remote_root_for(instance);
    let root = if home == "/" {
        format!("/{rel_root}")
    } else {
        format!("{}/{rel_root}", home.trim_end_matches('/'))
    };
    let outbox = am_base::outbox_remote::remote_dir(&home, instance, bot_id)?;
    Some(RemoteSite {
        conn,
        host: host.to_string(),
        home,
        root,
        workspace,
        outbox,
    })
}
