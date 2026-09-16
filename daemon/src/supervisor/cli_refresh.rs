//! 開機時刷新已安裝的 `bin/agm`（SPEC §18.2a）。
//!
//! `bin/agm` 是內嵌在 binary 裡的 [`super::setup::AGM_CLI`]，以前只在 supervisor／responder setup 寫出，
//! 換版不會重寫——2026-09-16 線上 daemon 已經要求 lease_token，裝好的 CLI 卻沒有這個參數，租約還不回去。
//!
//! 規則寫死在這裡：**只動 `bin/agm`**。setup 會一併重寫 `CLAUDE.md`／`persona.md`／`runtime.json`，
//! 協調者那條還會重設身分／model／effort，開機不能走那條。沒設定的角色、不存在的目錄一律跳過，不代建。
//! 寫不進去只記 warn 並推一則 inbox，daemon 照樣開機。`scripts/ops/*.sh` 沒有內嵌，仍要手動安裝。

use crate::state::App;
use serde_json::json;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::roles::{self, Role};
use super::store;

/// 內容指紋：FNV-1a 64，取 12 碼十六進位。不能用 `DefaultHasher`——它不保證跨版本穩定，
/// 備份檔名會變，「同一個雜湊只留一份」就失效了。
pub fn short_hash(content: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in content {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 目錄或 `bin/` 不存在：沒設定好的環境不代建。
    Skipped,
    Unchanged,
    Refreshed { old_hash: Option<String>, new_hash: String, backup: Option<String> },
}

/// 比對 `<dir>/bin/agm` 與 `embedded`，不同就備份舊檔（同雜湊已備份過不重複）再原子寫入（tmp＋rename，0755）。
pub fn refresh_cli(dir: &Path, embedded: &str) -> std::io::Result<Outcome> {
    let bin_dir = dir.join("bin");
    if !bin_dir.is_dir() {
        return Ok(Outcome::Skipped);
    }
    let bin = bin_dir.join("agm");
    let old = match std::fs::read(&bin) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if old.as_deref() == Some(embedded.as_bytes()) {
        return Ok(Outcome::Unchanged);
    }
    let old_hash = old.as_deref().map(short_hash);
    let mut backup = None;
    if let Some(h) = &old_hash {
        let name = format!("agm.bak-{h}");
        let path = bin_dir.join(&name);
        if !path.exists() {
            std::fs::copy(&bin, &path)?;
        }
        backup = Some(name);
    }
    let tmp = bin_dir.join(format!(".agm.tmp-{}", std::process::id()));
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(embedded.as_bytes())?;
        f.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, &bin)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(Outcome::Refreshed { old_hash, new_hash: short_hash(embedded.as_bytes()), backup })
}

/// 已設定的角色與它的工作目錄。沒設定（沒有 bot）就不在清單裡。
async fn configured_dirs(app: &Arc<App>) -> Vec<(Role, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(sup) = store::get_or_init(&app.db).await {
        if sup.bot_id.is_some() {
            out.push((Role::Patrol, sup.cwd.map(PathBuf::from).unwrap_or_else(|| super::setup::agm_dir(app))));
        }
    }
    if let Ok(r) = roles::get(&app.db, Role::Responder).await {
        if r.bot_id.is_some() {
            out.push((Role::Responder, r.cwd.map(PathBuf::from).unwrap_or_else(|| super::responder::dir(app))));
        }
    }
    out
}

pub async fn refresh_on_startup(app: &Arc<App>) {
    refresh_with(app, super::setup::AGM_CLI).await
}

async fn refresh_with(app: &Arc<App>, embedded: &str) {
    for (role, dir) in configured_dirs(app).await {
        match refresh_cli(&dir, embedded) {
            Ok(Outcome::Refreshed { old_hash, new_hash, backup }) => tracing::info!(
                role = role.as_str(),
                old = old_hash.as_deref().unwrap_or("-"),
                new = %new_hash,
                backup = backup.as_deref().unwrap_or("-"),
                "daemon started: refreshed the installed bin/agm"
            ),
            Ok(_) => {}
            Err(e) => {
                let new_hash = short_hash(embedded.as_bytes());
                tracing::warn!(role = role.as_str(), dir = %dir.display(), error = %e, "daemon started: could not refresh bin/agm; the installed CLI is stale");
                let _ = store::push_inbox(
                    &app.db,
                    &format!("agm_cli_stale:{}:{new_hash}", role.as_str()),
                    "agm_cli_stale",
                    None,
                    None,
                    None,
                    &json!({
                        "role": role.as_str(),
                        "path": dir.join("bin").join("agm").to_string_lossy(),
                        "embedded_hash": new_hash,
                        "error": e.to_string(),
                        "action": "CLI 沒更新：修好目錄權限後重啟 daemon，或手動把 scripts/agm.py 裝成 bin/agm",
                    }),
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("am-cli-refresh-{}", crate::db::ulid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn with_bin(content: &str) -> PathBuf {
        let d = tmpdir();
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::write(d.join("bin/agm"), content).unwrap();
        for (f, body) in [("CLAUDE.md", "claude"), ("persona.md", "persona"), ("runtime.json", "{\"model\":\"x\"}")] {
            std::fs::write(d.join(f), body).unwrap();
        }
        d
    }

    fn baks(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d.join("bin"))
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with("agm.bak-"))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn same_content_is_not_rewritten() {
        let d = with_bin("NEW");
        let before = std::fs::metadata(d.join("bin/agm")).unwrap().modified().unwrap();
        assert_eq!(refresh_cli(&d, "NEW").unwrap(), Outcome::Unchanged);
        assert_eq!(std::fs::metadata(d.join("bin/agm")).unwrap().modified().unwrap(), before);
        assert!(baks(&d).is_empty());
    }

    #[test]
    fn different_content_is_backed_up_and_replaced_executable_and_nothing_else_moves() {
        let d = with_bin("OLD");
        let out = refresh_cli(&d, "NEW").unwrap();
        let name = format!("agm.bak-{}", short_hash(b"OLD"));
        assert_eq!(
            out,
            Outcome::Refreshed { old_hash: Some(short_hash(b"OLD")), new_hash: short_hash(b"NEW"), backup: Some(name.clone()) }
        );
        assert_eq!(std::fs::read_to_string(d.join("bin/agm")).unwrap(), "NEW");
        assert_eq!(std::fs::read_to_string(d.join("bin").join(&name)).unwrap(), "OLD");
        assert_eq!(std::fs::metadata(d.join("bin/agm")).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(std::fs::read_to_string(d.join("CLAUDE.md")).unwrap(), "claude");
        assert_eq!(std::fs::read_to_string(d.join("persona.md")).unwrap(), "persona");
        assert_eq!(std::fs::read_to_string(d.join("runtime.json")).unwrap(), "{\"model\":\"x\"}");
        assert!(!std::fs::read_dir(d.join("bin")).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".agm.tmp")));
    }

    #[test]
    fn the_same_old_version_is_backed_up_once() {
        let d = with_bin("OLD");
        refresh_cli(&d, "NEW").unwrap();
        let bak = d.join("bin").join(format!("agm.bak-{}", short_hash(b"OLD")));
        let stamp = std::fs::metadata(&bak).unwrap().modified().unwrap();
        // 有人手動裝回舊版，下一次開機又換掉：備份不重複、也不被覆寫。
        std::fs::write(d.join("bin/agm"), "OLD").unwrap();
        refresh_cli(&d, "NEWER").unwrap();
        assert_eq!(baks(&d), vec![format!("agm.bak-{}", short_hash(b"OLD"))]);
        assert_eq!(std::fs::metadata(&bak).unwrap().modified().unwrap(), stamp);
        assert_eq!(std::fs::read_to_string(d.join("bin/agm")).unwrap(), "NEWER");
    }

    #[test]
    fn a_missing_directory_is_skipped_not_created() {
        let d = tmpdir().join("never-set-up");
        assert_eq!(refresh_cli(&d, "NEW").unwrap(), Outcome::Skipped);
        assert!(!d.exists());
        let bare = tmpdir();
        assert_eq!(refresh_cli(&bare, "NEW").unwrap(), Outcome::Skipped);
        assert!(!bare.join("bin").exists());
    }

    #[test]
    fn the_hash_is_stable_across_builds() {
        assert_eq!(short_hash(b""), "cbf29ce48422");
        assert_eq!(short_hash(b"a"), "af63dc4c8601");
    }

    #[tokio::test]
    async fn startup_refreshes_configured_roles_only_and_a_write_failure_does_not_stop_boot() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let patrol = with_bin("OLD");
        let responder = with_bin("OLD");
        store::get_or_init(&app.db).await.unwrap();
        store::set_env(&app.db, "patrol-bot", &e.project_id, &patrol.to_string_lossy()).await.unwrap();
        // 協調者沒設定：它的目錄就算在也不動。
        refresh_with(app, "NEW").await;
        assert_eq!(std::fs::read_to_string(patrol.join("bin/agm")).unwrap(), "NEW");
        assert_eq!(std::fs::read_to_string(responder.join("bin/agm")).unwrap(), "OLD");

        roles::set_env(&app.db, Role::Responder, "responder-bot", &e.project_id, &responder.to_string_lossy()).await.unwrap();
        let before = roles::get(&app.db, Role::Responder).await.unwrap();
        let bin = responder.join("bin");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
        refresh_with(app, "NEW").await; // 不 panic、不回錯：開機照走
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(std::fs::read_to_string(bin.join("agm")).unwrap(), "OLD");
        assert_eq!(std::fs::read_to_string(responder.join("persona.md")).unwrap(), "persona");

        let kinds: Vec<(String, String)> = sqlx::query_as("SELECT kind, payload_json FROM supervisor_inbox WHERE kind='agm_cli_stale'")
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(kinds.len(), 1, "{kinds:?}");
        assert!(kinds[0].1.contains("\"role\":\"responder\""), "{}", kinds[0].1);
        let after = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!((after.identity, after.model, after.effort), (before.identity, before.model, before.effort));
    }
}
