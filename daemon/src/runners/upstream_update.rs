use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use futures::future::BoxFuture;
use serde_json::{json, Value};

use crate::changelog;
use crate::state::App;
use crate::upstream_update::{
    codex_latest, grok_latest, herdr_latest, item_json, last_path, npm_latest, tick,
    Sources, GROK_STABLE_URL, HERDR_RELEASES_API, NPM_CLAUDE_LATEST, SWEEP,
};

pub struct Live(pub Arc<App>);

impl Sources for Live {
    fn upstream<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            if kind == "codex" {
                return codex_latest(&changelog::fetch_changelog(&self.0, "codex").await?);
            }
            let client = reqwest::Client::builder()
                .user_agent("agents-manager")
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|e| anyhow!("http client: {e}"))?;
            if kind == "herdr" {
                let resp = client.get(HERDR_RELEASES_API).send().await.map_err(|e| anyhow!("連不上 GitHub releases：{e}"))?;
                if !resp.status().is_success() {
                    return Err(anyhow!("GitHub releases 回 HTTP {}", resp.status()));
                }
                return herdr_latest(&resp.text().await.map_err(|e| anyhow!("讀 GitHub releases 回應失敗：{e}"))?);
            }
            if kind == "grok" {
                let resp = client.get(GROK_STABLE_URL).send().await.map_err(|e| anyhow!("連不上 grok 的發佈位置：{e}"))?;
                if !resp.status().is_success() {
                    return Err(anyhow!("grok 的發佈位置回 HTTP {}", resp.status()));
                }
                return grok_latest(&resp.text().await.map_err(|e| anyhow!("讀 grok 的發佈位置回應失敗：{e}"))?);
            }
            let resp = client.get(NPM_CLAUDE_LATEST).send().await.map_err(|e| anyhow!("連不上 npm registry：{e}"))?;
            if !resp.status().is_success() {
                return Err(anyhow!("npm registry 回 HTTP {}", resp.status()));
            }
            npm_latest(&resp.text().await.map_err(|e| anyhow!("讀 npm registry 回應失敗：{e}"))?)
        })
    }

    fn hosts<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move {
            // 先拿主機清單再鎖 tools：不要拿著 tools 鎖去等別的鎖。
            let conns = self.0.hosts.list().await;
            let tools = self.0.tools.lock().await;
            let mut out = Vec::new();
            for c in conns {
                let installed = match tools.get(&c.name) {
                    Some(t) if kind == "herdr" => t.herdr_cli.is_some(),
                    Some(t) => t.tools.get(kind).is_some_and(|t| t.installed),
                    None => false,
                };
                if installed {
                    out.push(c.name.clone());
                }
            }
            out
        })
    }

    fn installed<'a>(&'a self, host: &'a str, kind: &'a str) -> BoxFuture<'a, Result<String>> {
        if kind == "herdr" {
            return Box::pin(async move {
                self.0.tools.lock().await.get(host).and_then(|t| t.herdr_cli.clone()).ok_or_else(|| anyhow!("讀不到 `herdr --version`"))
            });
        }
        Box::pin(changelog::installed_version(&self.0, host, kind))
    }
}

pub fn spawn(app: Arc<App>) {
    // 開機先等工具探測跑完（第一輪延後 60 秒），不然第一輪一台主機都沒有。
    crate::background_loop::spawn_periodic(&app, "upstream update watcher", SWEEP, Duration::from_secs(60), |app| async move {
        let src = Live(app.clone());
        let path = last_path(&app);
        tick(&app, &app.upstream_watch, &src, &path).await;
    });
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/upstream-updates", get(get_status))
}

/// `GET /api/upstream-updates`：最近一輪的快照，不觸發抓取。第一輪還沒跑完是空陣列。
async fn get_status(State(app): State<Arc<App>>) -> Json<Value> {
    let items: Vec<Value> = app.upstream_watch.snapshot().await.iter().map(item_json).collect();
    Json(json!({ "items": items }))
}
