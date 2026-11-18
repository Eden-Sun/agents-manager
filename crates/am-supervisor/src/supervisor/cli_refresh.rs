//! 開機時刷新已安裝的 `bin/agm`（SPEC §18.2a）。
//!
//! `bin/agm` 是內嵌在 binary 裡的 [`super::setup::AGM_CLI`]，以前只在 supervisor／responder setup 寫出，
//! 換版不會重寫——2026-09-16 線上 daemon 已經要求 lease_token，裝好的 CLI 卻沒有這個參數，租約還不回去。
//!
//! 規則寫死在這裡：**只動 `bin/agm`**。setup 會一併重寫 `CLAUDE.md`／`persona.md`／`runtime.json`，
//! 協調者那條還會重設身份／model／effort，開機不能走那條。沒設定的角色、不存在的目錄一律跳過，不代建。
//! 寫不進去只記 warn 並推一則 inbox，daemon 照樣開機。`scripts/ops/*.sh` 沒有內嵌，仍要手動安裝。

#[cfg(all(test, feature = "daemon-test-harness"))]
use axum::extract::State;
#[cfg(all(test, feature = "daemon-test-harness"))]
use axum::Json;
use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::roles::{self, Role};
use super::store;

pub use crate::supervisor_inbox::short_hash;

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
        let mode = std::fs::metadata(&bin)?.permissions().mode() & 0o777;
        if mode == 0o755 {
            return Ok(Outcome::Unchanged);
        }
        let hash = short_hash(embedded.as_bytes());
        install_executable(&bin, embedded.as_bytes())?;
        return Ok(Outcome::Refreshed { old_hash: Some(hash.clone()), new_hash: hash, backup: None });
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
async fn configured_dirs(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db)) -> Vec<(Role, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(sup) = store::get_or_init(app.db()).await {
        if sup.bot_id.is_some() {
            out.push((Role::Patrol, sup.cwd.map(PathBuf::from).unwrap_or_else(|| super::setup::agm_dir(app))));
        }
    }
    if let Ok(r) = roles::get(app.db(), Role::Responder).await {
        if r.bot_id.is_some() {
            out.push((Role::Responder, r.cwd.map(PathBuf::from).unwrap_or_else(|| super::responder::dir(app))));
        }
    }
    out
}

pub async fn refresh_on_startup(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db)) {
    let _ = refresh_with(app, super::setup::AGM_CLI).await;
}

/// 已安裝的 `bin/agm` 跟這顆 binary 內嵌的那份對不對得上（issue #532）。
///
/// `agm ops-sync --check` 只比對**已安裝的 ops 腳本**與 repo，`bin/agm` 完全不在它的視野裡
/// （`install-manifest.tsv` 明寫「bin/agm 由 daemon 部署，不在這裡」）。於是「腳本比 CLI 新」
/// 這個組合一律回報 ok，而它正是最會痛的那一種：`daemon-update-kick.sh` 派出的正文用
/// `--lease-token-file`，舊的 `bin/agm` 不認得就 argparse rc 2，rebuild 窗口沒交還、握到 TTL。
pub async fn status(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db), embedded: &str) -> Value {
    let embedded_hash = short_hash(embedded.as_bytes());
    let mut roles = Vec::new();
    for (role, dir) in configured_dirs(app).await {
        let bin_dir = dir.join("bin");
        let path = bin_dir.join("agm");
        let installed_hash = std::fs::read(&path).ok().map(|b| short_hash(&b));
        let executable = std::fs::metadata(&path)
            .ok()
            .is_some_and(|metadata| metadata.permissions().mode() & 0o777 == 0o755);
        let state = match &installed_hash {
            Some(h) if *h == embedded_hash && executable => "ok",
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

/// 每個已設定的角色刷新一次，回傳各角色的結果：`{role, outcome, error?}`（`outcome` 是 `refreshed`／
/// `unchanged`／`skipped`／`failed`，`error` 只在 failed 有）。失敗的原因給 `POST /api/supervisor/cli` 回給呼叫的人看。
pub async fn refresh_with(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db), embedded: &str) -> Vec<Value> {
    let mut outcomes = Vec::new();
    for (role, dir) in configured_dirs(app).await {
        match refresh_cli(&dir, embedded) {
            Ok(Outcome::Refreshed { old_hash, new_hash, backup }) => {
                tracing::info!(
                    role = role.as_str(),
                    old = old_hash.as_deref().unwrap_or("-"),
                    new = %new_hash,
                    backup = backup.as_deref().unwrap_or("-"),
                    "refreshed the installed bin/agm"
                );
                outcomes.push(json!({"role": role.as_str(), "outcome": "refreshed"}));
            }
            Ok(Outcome::Unchanged) => outcomes.push(json!({"role": role.as_str(), "outcome": "unchanged"})),
            Ok(Outcome::Skipped) => outcomes.push(json!({"role": role.as_str(), "outcome": "skipped"})),
            Err(e) => {
                let new_hash = short_hash(embedded.as_bytes());
                tracing::warn!(role = role.as_str(), dir = %dir.display(), error = %e, "could not refresh bin/agm; the installed CLI is stale");
                outcomes.push(json!({"role": role.as_str(), "outcome": "failed", "error": e.to_string()}));
                let _ = store::push_inbox(
                    app.db(),
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
    outcomes
}

#[cfg(all(test, feature = "daemon-test-harness"))]
pub use crate::runners::supervisor::cli::get_cli;
