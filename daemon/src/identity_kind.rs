//! 身分（`cc0`／`cc1`／`cc2`…）是**有 kind 的**（SPEC §16）。
//!
//! 2026-09-14 使用者指正：cc0／cc1／cc2 是 Claude Code 的帳號代號，跟 codex 無關，codex 不該有任何
//! `codex:ccN` 的 quota key。實際根因是 codex 子 agent 從 claude 母 bot **原封不動抄了母 bot 的
//! identity**（onvifdev、rtsp 帶 cc0；pt-hub-aawpak-ops 帶 cc1），quota 寫入用 `<kind>:<identity>`
//! 就生出 `codex:cc1`，額度列多一格、數字還跟真的 codex 帳號分家。
//!
//! 使用者建的 bot 早就有這道檢查（`POST /api/projects/{id}/bots`、`PATCH /api/bots/{id}` 對
//! kind 不符回 400），漏掉的是 daemon 自己收編子 agent 的那條路。這裡三件事：
//!
//! 1. [`child_identity`]：收編時只繼承**同 kind** 母 bot 的 identity。
//! 2. `quota::quota_base_for_host` 在 identity 的 kind 跟 bot 不同時寫裸 kind（見那邊）。
//! 3. [`cleanup_host`]：每次偵測完一台主機的身分，清掉既有 bot 身上 kind 不符的 identity，以及
//!    quota 表裡因此留下的殘留 key。

use crate::state::App;
use std::sync::Arc;

/// 子 agent 從母 bot 繼承的 identity：同 kind 才繼承，不同 kind 一律不帶（由 CLI 自己的預設帳號跑，
/// 之後 `pane_identity::sync_child_identity` 會照它 pane 真正的帳號目錄補回同 kind 的身分）。
pub fn child_identity(parent_identity: Option<&str>, parent_kind: &str, child_kind: &str) -> Option<String> {
    if parent_kind != child_kind {
        return None;
    }
    parent_identity.map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// 清理一台主機：kind 不符的 identity 設回 NULL、quota 表裡 `<kind>:<ccN>`（ccN 是別的 kind 的身分）
/// 那種殘留 key 刪掉。回傳（清掉幾顆 bot, 刪掉幾把 key）。
///
/// 只在這台主機的身分**已經偵測到**時才動手：偵測前 `identities_for_host` 可能只有 config 那幾筆，
/// 拿不完整的表去判「不符」會誤殺。使用者自己建的 bot（`managed_by = 'user'`）的 identity 是
/// `config.toml` 的設定、會被投影寫回，daemon 不從這裡改它，只記 warn——那種組合本來就過不了
/// API 的檢查，出現了代表有人手改設定檔。
pub async fn cleanup_host(app: &Arc<App>, host: &str) -> (usize, usize) {
    let identities = crate::tools::identities_for_host(app, host).await;
    if identities.is_empty() {
        return (0, 0);
    }
    let kind_of = |name: &str| identities.iter().find(|i| i.name == name).map(|i| i.kind.clone());

    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT b.id, b.name, b.kind, b.identity, b.managed_by FROM bots b JOIN projects p ON p.id = b.project_id
          WHERE p.host = ? AND b.deleted_at IS NULL AND b.identity IS NOT NULL AND b.identity != ''",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await
    .unwrap_or_default();
    let mut cleared = 0;
    for (id, name, kind, identity, managed_by) in rows {
        let Some(id_kind) = kind_of(&identity) else { continue };
        if id_kind == kind {
            continue;
        }
        if managed_by == "user" {
            tracing::warn!(host, bot = %name, %identity, identity_kind = %id_kind, bot_kind = %kind,
                           "a user bot carries an identity of another kind; fix it in config.toml");
            continue;
        }
        let res = sqlx::query("UPDATE bots SET identity = NULL WHERE id = ? AND identity = ? AND managed_by != 'user'")
            .bind(&id)
            .bind(&identity)
            .execute(&app.db)
            .await;
        if matches!(res, Ok(r) if r.rows_affected() > 0) {
            cleared += 1;
            tracing::info!(host, bot = %name, %identity, identity_kind = %id_kind, bot_kind = %kind,
                           "cleared an identity of another kind from a bot");
            app.emit("bot_changed", serde_json::json!({"bot_id": id})).await;
        }
    }

    // quota 表：`<host>/<kind>:<name>`（或本機的 `<kind>:<name>`），name 是別的 kind 的身分 → 殘留。
    let mut removed = Vec::new();
    {
        let mut quotas = app.quotas.lock().await;
        let prefix = if host == crate::config::LOCAL_HOST { String::new() } else { format!("{host}/") };
        let stale: Vec<String> = quotas
            .keys()
            .filter_map(|k| {
                let base = if prefix.is_empty() {
                    if k.contains('/') { return None } else { k.as_str() }
                } else {
                    k.strip_prefix(&prefix)?
                };
                let (kind, name) = base.split_once(':')?;
                (kind_of(name).is_some_and(|ik| ik != kind)).then(|| k.clone())
            })
            .collect();
        for k in stale {
            quotas.remove(&k);
            removed.push(k);
        }
    }
    for k in &removed {
        tracing::info!(host, key = %k, "removed a quota key for an identity of another kind");
        app.emit("quota_updated", serde_json::json!({"kind": k, "host": host, "quota": null})).await;
    }
    (cleared, removed.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_inherits_the_parents_identity_only_when_the_kind_matches() {
        assert_eq!(child_identity(Some("cc1"), "claude", "claude").as_deref(), Some("cc1"));
        // 2026-09-14：codex 子 agent 從 claude 母 bot 抄到 cc1，quota 就長出 `codex:cc1`。
        assert_eq!(child_identity(Some("cc1"), "claude", "codex"), None);
        assert_eq!(child_identity(Some("cc0"), "claude", "grok"), None);
        assert_eq!(child_identity(None, "claude", "claude"), None);
        assert_eq!(child_identity(Some("  "), "claude", "claude"), None);
    }

    fn ident(name: &str, kind: &str) -> crate::config::IdentityCfg {
        crate::config::IdentityCfg { name: name.into(), kind: kind.into(), host: None, env: Default::default(), args: vec![] }
    }

    #[tokio::test]
    async fn startup_cleanup_drops_cross_kind_identities_and_their_quota_keys() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![ident("cc0", "claude"), ident("cc1", "claude")],
                checked_at: crate::db::now(),
            },
        );
        let bot = |name: &str, kind: &str, identity: &str, managed_by: &str| {
            let app = app.clone();
            let project = env.project_id.clone();
            let (name, kind, identity, managed_by) = (name.to_string(), kind.to_string(), identity.to_string(), managed_by.to_string());
            async move {
                let id = crate::db::ulid();
                sqlx::query(
                    "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, identity, managed_by, created_at)
                     VALUES (?,?,?,?,'[]',0,1,'tok',?,?,?)",
                )
                .bind(&id)
                .bind(&project)
                .bind(&name)
                .bind(&kind)
                .bind(&identity)
                .bind(&managed_by)
                .bind(crate::db::now())
                .execute(&app.db)
                .await
                .unwrap();
                id
            }
        };
        let rtsp = bot("rtsp", "codex", "cc1", "child").await;
        let keep = bot("am-claude", "claude", "cc1", "child").await;
        let user = bot("user-codex", "codex", "cc0", "user").await;
        let empty = |src: &str| crate::quota::Quota {
            five_hour: None, seven_day: None, fable: None, reset_credits: None, limit_hit: None, plan: None,
            updated_at: crate::db::now(), source: src.into(), account: None, host: crate::config::LOCAL_HOST.into(),
        };
        {
            let mut q = app.quotas.lock().await;
            q.insert("codex:cc1".into(), empty("codex-statusline"));
            q.insert("codex".into(), empty("codex-app-server"));
            q.insert("claude:cc1".into(), empty("statusline"));
        }

        let (cleared, removed) = cleanup_host(&app, crate::config::LOCAL_HOST).await;
        assert_eq!((cleared, removed), (1, 1));
        let id_of = |b: &str| {
            let app = app.clone();
            let b = b.to_string();
            async move { crate::db::bot(&app.db, &b).await.unwrap().unwrap().identity }
        };
        assert_eq!(id_of(&rtsp).await, None, "codex 子 agent 身上的 claude 身分清掉");
        assert_eq!(id_of(&keep).await.as_deref(), Some("cc1"), "同 kind 的留著");
        assert_eq!(id_of(&user).await.as_deref(), Some("cc0"), "使用者的設定不從這裡改，只記 warn");
        let q = app.quotas.lock().await;
        assert!(!q.contains_key("codex:cc1"), "殘留的 codex:cc1 刪掉");
        assert!(q.contains_key("codex") && q.contains_key("claude:cc1"), "其他 key 不動");
    }
}
