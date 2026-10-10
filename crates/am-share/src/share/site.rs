//! 分享 bot 檔案位置解析（SPEC §20、remote-share-design §3.3）。
//!
//! 把 `shared_bots.workspace` 與 `projects.host` 解析為這顆分享 bot 的實際檔案所在位置。
//! 本機為 `ShareSite::Local`，遠端主機為 `ShareSite::Remote`。
//! 任何不確定（DB 查詢錯誤、主機未連線、路徑不合法）一律 fail closed 回 `SiteError::Unavailable`。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::time::Duration;

use am_base::hosts::{HostConn, HostFence, SshStream};
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

/// 主機連線的圍籬視圖（#1026）：遠端分享的 I/O 只能經過這裡，每一次 ssh 都在 [`HostFence::run_current`] 之內送出，
/// 所以 repoint／重連之後舊的分享位置不會再把位元組寫進舊主機（拿不到權威就是 `Err`，呼叫端對外一律 503）。
#[derive(Clone)]
pub struct FencedConn {
    fence: HostFence,
}

impl FencedConn {
    pub fn new(fence: HostFence) -> Self {
        Self { fence }
    }

    pub fn fence(&self) -> &HostFence {
        &self.fence
    }

    /// 只給讀取旗標與設定用（例如 `connected`、`remote_path`）；I/O 一律走本型別的方法，不會繞過圍籬。
    pub fn conn(&self) -> &Arc<HostConn> {
        self.fence.conn()
    }

    pub fn name(&self) -> &str {
        &self.fence.conn().name
    }

    pub fn instance(&self) -> Option<&str> {
        self.fence.conn().instance()
    }

    pub fn remote_path(&self) -> String {
        self.fence.conn().remote_path()
    }

    pub fn is_connected(&self) -> bool {
        self.fence.conn().is_connected()
    }

    async fn fenced<T>(&self, op: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
        match self.fence.run_current(op).await {
            Some(out) => out,
            None => Err(anyhow::anyhow!("host `{}` authority superseded; the share site must be re-resolved", self.name())),
        }
    }

    pub async fn home(&self) -> anyhow::Result<String> {
        self.fenced(self.fence.conn().home()).await
    }

    pub async fn ssh_exec_timeout(&self, script: &str, timeout: Duration) -> anyhow::Result<String> {
        self.fenced(self.fence.conn().ssh_exec_timeout(script, timeout)).await
    }

    pub async fn ssh_exec_path_timeout(&self, script: &str, timeout: Duration) -> anyhow::Result<String> {
        self.fenced(self.fence.conn().ssh_exec_path_timeout(script, timeout)).await
    }

    pub async fn ssh_exec_stdin(&self, script: &str, data: &[u8], timeout: Duration) -> anyhow::Result<String> {
        self.fenced(self.fence.conn().ssh_exec_stdin(script, data, timeout)).await
    }

    pub async fn ssh_stream(&self, script: &str, stdin: &[u8]) -> anyhow::Result<SshStream> {
        self.fenced(self.fence.conn().ssh_stream(script, stdin)).await
    }
}

#[derive(Clone)]
pub struct RemoteSite {
    pub conn: FencedConn,
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
    /// 這台主機目前的權威圍籬（#1026）：解析分享位置時一起抓住，之後的 I/O 都綁在這一代上。
    fn host_fence(&self, host: &str) -> impl Future<Output = Option<HostFence>> + Send;
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

    fn host_fence(&self, host: &str) -> impl Future<Output = Option<HostFence>> + Send {
        (**self).host_fence(host)
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
        let conn = FencedConn::new(app.host_fence(&host).await.ok_or(SiteError::Unavailable)?);
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
    conn: FencedConn,
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
