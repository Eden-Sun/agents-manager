//! 開機時刷新已安裝的 `bin/agm`（SPEC §18.2a）。
//!
//! `bin/agm` 是內嵌在 binary 裡的 [`super::setup::AGM_CLI`]，以前只在 supervisor／responder setup 寫出，
//! 換版不會重寫——2026-09-16 線上 daemon 已經要求 lease_token，裝好的 CLI 卻沒有這個參數，租約還不回去。
//!
//! 規則寫死在這裡：**只動 `bin/agm`**。setup 會一併重寫 `CLAUDE.md`／`persona.md`／`runtime.json`，
//! 協調者那條還會重設身分／model／effort，開機不能走那條。沒設定的角色、不存在的目錄一律跳過，不代建。
//! 寫不進去只記 warn 並推一則 inbox，daemon 照樣開機。`scripts/ops/*.sh` 沒有內嵌，仍要手動安裝。

use crate::state::App;
use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};
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

/// 原子地裝一個**可執行檔**：暫存檔 → `fchmod` → `rename`（issue #520）。
///
/// `bin/agm` 是一直有人在跑的東西（`agm.py`，所有 kick 每 5～30 分鐘 exec 一次）。`std::fs::write`
/// 是 `O_TRUNC` 之後才寫，中間那一段別人讀到的是空的或半截——python 啟動時整個讀檔，剛好落在那裡
/// 就是 `SyntaxError`。`rename` 在同一個檔案系統上是原子的，所以讀的人永遠只會看到**完整的舊版或
/// 完整的新版**，不會有中間狀態。
///
/// 權限用 **fd 上的 `fchmod`**（`File::set_permissions`）而不是對路徑 `chmod`：對路徑做的話，
/// 「建檔」跟「補上 +x」之間有一段檔案存在但還不能執行的窗口（新檔是 umask 的 0644）。先在暫存檔上
/// 設好、再 `rename` 進去，那個檔一出現在最終路徑就已經是 0755。
///
/// 暫存檔名帶 pid ＋ 單調遞增的序號：同一個行程裡兩個呼叫（`deploy_files` 與 `refresh_cli`）可能同時
/// 寫同一個目錄，只用 pid 會互相蓋掉對方的暫存檔。
pub fn install_executable(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{} 沒有上層目錄", path.display())))?;
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("agm");
    let tmp = dir.join(format!(".{name}.tmp-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        // fd 上設權限：rename 進去的那一刻就已經是 0755，沒有「存在但不能執行」的窗口。
        f.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
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
    install_executable(&bin, embedded.as_bytes())?;
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

/// 已安裝的 `bin/agm` 跟這顆 binary 內嵌的那份對不對得上（issue #532）。
///
/// `agm ops-sync --check` 只比對**已安裝的 ops 腳本**與 repo，`bin/agm` 完全不在它的視野裡
/// （`install-manifest.tsv` 明寫「bin/agm 由 daemon 部署，不在這裡」）。於是「腳本比 CLI 新」
/// 這個組合一律回報 ok，而它正是最會痛的那一種：`daemon-update-kick.sh` 派出的正文用
/// `--lease-token-file`，舊的 `bin/agm` 不認得就 argparse rc 2，rebuild 窗口沒交還、握到 TTL。
async fn status(app: &Arc<App>, embedded: &str) -> Value {
    let embedded_hash = short_hash(embedded.as_bytes());
    let mut roles = Vec::new();
    for (role, dir) in configured_dirs(app).await {
        let bin_dir = dir.join("bin");
        let path = bin_dir.join("agm");
        let installed_hash = std::fs::read(&path).ok().map(|b| short_hash(&b));
        let state = match &installed_hash {
            Some(h) if *h == embedded_hash => "ok",
            // 換得動：`POST /api/supervisor/cli` 或下一次開機就會補上，不必重建 binary。
            Some(_) => "stale",
            None if bin_dir.is_dir() => "missing",
            None => "not_set_up",
        };
        roles.push(json!({
            "role": role.as_str(),
            "dir": dir.to_string_lossy(),
            "path": path.to_string_lossy(),
            "installed_hash": installed_hash,
            "state": state,
        }));
    }
    json!({"embedded_hash": embedded_hash, "roles": roles})
}

/// `GET /api/supervisor/cli`。
pub async fn get_cli(State(app): State<Arc<App>>) -> Json<Value> {
    Json(status(&app, super::setup::AGM_CLI).await)
}

/// `POST /api/supervisor/cli`：不等下一次開機，現在就把 `bin/agm` 換成內嵌的那份（issue #532）。
///
/// 開機是唯一觸發點的時候，「安裝端落後」這件事只能靠重啟 daemon 修——而重啟要另外申請核准，
/// 於是一個換個檔案就好的問題被綁在整條換版流程上。這條路只做 `refresh_on_startup` 做的事
/// （只動 `bin/agm`，不碰 persona／runtime.json），回傳更新後的狀態。
pub async fn post_cli_refresh(State(app): State<Arc<App>>) -> Json<Value> {
    refresh_with(&app, super::setup::AGM_CLI).await;
    Json(status(&app, super::setup::AGM_CLI).await)
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

    /// issue #520：`bin/agm` 是一直有人在 exec 的 python 腳本。以前 `deploy_files` 用 `fs::write`
    /// （`O_TRUNC` 之後才寫），讀的人會看到空的或半截。這條真的開一個讀者執行緒一直讀，一邊反覆換版：
    /// **每一次讀到的內容都必須是完整的舊版或完整的新版**，不能有第三種。
    #[test]
    fn a_reader_only_ever_sees_a_whole_0755_executable() {
        let dir = tmpdir();
        let bin = dir.join("agm");
        // 夠大才看得出截斷：小檔案有機會在一次 write 裡寫完，測不到那個窗口。
        let old: String = "# old\n".repeat(20_000);
        let new: String = "# new\n".repeat(20_000);
        install_executable(&bin, old.as_bytes()).unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let reader = {
            let (bin, stop, seen) = (bin.clone(), stop.clone(), seen.clone());
            let (old, new) = (old.clone(), new.clone());
            std::thread::spawn(move || {
                let mut reads = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // 權限跟內容一起看：檔案只要出現在最終路徑，就該已經是 0755。
                    // 對路徑 chmod 的寫法在「建檔」與「補 +x」之間會有一瞬是 umask 的 0644。
                    if let Ok(md) = std::fs::metadata(&bin) {
                        let mode = md.permissions().mode() & 0o777;
                        if mode != 0o755 {
                            seen.lock().unwrap().push(format!("權限 {mode:o}"));
                        }
                    }
                    if let Ok(got) = std::fs::read_to_string(&bin) {
                        reads += 1;
                        // 只記「不是舊也不是新」的那種，記字數就夠指認是不是半截。
                        if got != *"" && got.as_str() != old.as_str() && got.as_str() != new.as_str() {
                            seen.lock().unwrap().push(format!("{} 位元組", got.len()));
                        }
                    }
                }
                reads
            })
        };
        for i in 0..60 {
            let body = if i % 2 == 0 { &new } else { &old };
            // 每隔幾輪先刪掉：這樣才會走到「從無到有建檔」那條，權限窗口只在那裡出現。
            if i % 10 == 0 {
                let _ = std::fs::remove_file(&bin);
            }
            install_executable(&bin, body.as_bytes()).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let reads = reader.join().unwrap();

        let torn = seen.lock().unwrap().clone();
        assert!(torn.is_empty(), "讀到半截的內容、或權限不是 0755 的瞬間：{torn:?}");
        assert!(reads > 0, "讀者一次都沒讀到，這條沒有真的驗到東西");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// issue #520：以前是 `fs::write` 建檔（umask，通常 0644）之後才對**路徑** chmod 0755，
    /// 中間那一瞬檔案已經在那裡但還不能執行。`fchmod` 在暫存檔上設好再 rename，
    /// 所以那個檔一出現在最終路徑就是 0755——連第一次建立都沒有例外。
    #[test]
    fn the_executable_is_0755_the_moment_it_appears() {
        let dir = tmpdir();
        let bin = dir.join("agm");
        assert!(!bin.exists(), "前提：這是第一次建立");
        install_executable(&bin, b"#!/usr/bin/env python3\n").unwrap();
        let mode = std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "建出來就該是 0755，實際 {mode:o}");

        // 覆寫既有檔也要維持 0755：先把它改成 0644，再裝一次。
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        install_executable(&bin, b"#!/usr/bin/env python3\nprint(1)\n").unwrap();
        let mode = std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "覆寫之後也要是 0755，實際 {mode:o}");

        // 暫存檔不留下。
        let strays: Vec<String> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-")).collect();
        assert!(strays.is_empty(), "暫存檔沒清掉：{strays:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// #532：安裝端落後看得見，而且不必重啟 daemon 就修得掉。
    #[tokio::test]
    async fn the_api_reports_a_stale_cli_and_can_replace_it_without_a_restart() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let patrol = with_bin("OLD");
        store::get_or_init(&app.db).await.unwrap();
        store::set_env(&app.db, "patrol-bot", &e.project_id, &patrol.to_string_lossy()).await.unwrap();

        let st = status(app, "NEW").await;
        assert_eq!(st["embedded_hash"], short_hash(b"NEW"));
        assert_eq!(st["roles"][0]["role"], "patrol");
        assert_eq!(st["roles"][0]["state"], "stale", "{st}");
        assert_eq!(st["roles"][0]["installed_hash"], short_hash(b"OLD"));

        refresh_with(app, "NEW").await; // POST 走的就是這一支
        let st = status(app, "NEW").await;
        assert_eq!(st["roles"][0]["state"], "ok", "{st}");
        assert_eq!(std::fs::read_to_string(patrol.join("bin/agm")).unwrap(), "NEW");
        assert_eq!(std::fs::read_to_string(patrol.join("persona.md")).unwrap(), "persona", "只動 bin/agm");

        // GET 報的是這顆 binary 真的內嵌的那份（ops-sync 要拿它跟 repo 比）。
        let Json(v) = get_cli(State(app.clone())).await;
        assert_eq!(v["embedded_hash"], short_hash(super::super::setup::AGM_CLI.as_bytes()));

        // 沒有 bin/ 的角色目錄：not_set_up，不代建。
        let bare = tmpdir();
        roles::set_env(&app.db, Role::Responder, "responder-bot", &e.project_id, &bare.to_string_lossy()).await.unwrap();
        let st = status(app, "NEW").await;
        let responder = st["roles"].as_array().unwrap().iter().find(|r| r["role"] == "responder").unwrap().clone();
        assert_eq!(responder["state"], "not_set_up", "{st}");
        assert!(!bare.join("bin").exists());
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
