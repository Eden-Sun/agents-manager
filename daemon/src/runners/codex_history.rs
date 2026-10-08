//! Top-level adapter supplying the daemon services needed by Codex history.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::capabilities::{Cfg, Db};
use crate::codex_history::{CodexHistoryHost, CodexHistoryState, HistorySource};
use crate::config::LOCAL_HOST;
use crate::db;
use crate::state::App;

impl CodexHistoryHost for App {
    fn enabled(&self) -> impl Future<Output = bool> + Send + '_ {
        async move { self.cfg().get().await.codex_history.enabled }
    }

    fn source(&self) -> Option<Arc<dyn HistorySource>> {
        self.codex_history().get()
    }

    fn bot_host<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = Option<String>> + Send + 'a {
        async move { db::bot_host(self.db(), bot_id).await.ok() }
    }

    fn codex_home<'a>(&'a self, bot: &'a db::Bot) -> impl Future<Output = Option<PathBuf>> + Send + 'a {
        async move { crate::app_ports_p13::codex_home(&self.shared(), bot).await }
    }

    fn codex_program(&self) -> impl Future<Output = String> + Send + '_ {
        async move { crate::tools::cached_path(&self.shared(), LOCAL_HOST, "codex").await.unwrap_or_else(|| "codex".to_string()) }
    }
}
