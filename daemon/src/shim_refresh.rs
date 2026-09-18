//! 開機時把所有現存 bot 的 shim 換成**這顆 binary 帶的版本**（SPEC §6.5b／§6.5g）。
//!
//! shim（`bots/<id>/bin/herdr`、`bots/<id>/bin/cargo`）以前只在 **bot 啟動**時寫一次。長跑的 bot
//! ——AGM、協調者、使用者自己的專案 bot——可以好幾天不重啟 pane，於是新版 daemon 上線之後，
//! 它們手上還是舊 shim：
//!
//! * 2026-09-18 21:xx 巡檢實測：`ee98f6cf` 已經上線，AGM 那顆的 `bin/cargo` 還是 14:00 寫的舊版
//!   （沒有 `AM_SHIM_MARKER`），跑 `cargo check` 十分鐘不動，行程鏈是
//!   AGM shim → AGM shim → build shim → build shim → AGM shim——正是 shim 互相把對方當成真 cargo
//!   的那個巢狀死鎖，修正明明已經在 binary 裡。
//!
//! shim 只是一個檔案，換掉它不需要動 pane：**下一次**在 pane 裡打 `cargo` 就會執行到新版。正在
//! 跑的舊 shim 也不受影響——rename 只換目錄項，已經開啟的行程沿用舊 inode。
//!
//! 所以寫入一律「暫存檔 + rename」（[`write_atomic`]），而且**內容一樣就不重寫**：每次重啟都重寫
//! 會把 mtime 洗掉，之後沒人分得出哪些 shim 真的換過版。內容一樣但權限掉了（0644）的，只 chmod 回 0755。

use crate::state::App;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 一顆 bot 的 bin 目錄裡，我們負責的檔案與它現在該有的內容。
///
/// **本機開機掃描（[`refresh_all`]）與遠端同步（[`remote_sync_script`]）讀的是同一份**——兩邊不會各改各的、
/// 飄成兩套 shim（issue #124）。
pub(crate) fn shims() -> [(&'static str, &'static str); 2] {
    [("herdr", crate::herdr_shim::SHIM_SH), ("cargo", crate::cargo_shim::SHIM_SH)]
}

/// shim 該有的權限。內容跟權限是兩件事，各自判：光內容對不代表 pane 打得起來。
#[cfg(unix)]
const SHIM_MODE: u32 = 0o755;

/// 內容不同才寫，寫的時候先寫暫存檔再 rename（同一個目錄，所以 rename 是原子的）。
///
/// 回 `Ok(true)` ＝真的動了檔案（換了內容，或只修了權限）；`Ok(false)` ＝本來就對，什麼都沒碰。
///
/// 內容與權限**分開判**（issue #126）：從舊版升上來的檔案可能是 0644（`install_local` 以前寫完才 chmod，
/// 中間死掉就留下不能執行的 shim）。內容已經是現行版、只有權限不對時，只 chmod——不重寫內容，
/// mtime 與 inode 都不動；內容跟權限都對才是真的 no-op。
pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<bool> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return repair_mode(path);
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp-{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("shim"), std::process::id()));
    std::fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(SHIM_MODE))?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(true),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 內容已經對的檔案：權限不是 [`SHIM_MODE`] 就 chmod 回去（原子的，不必碰內容）。非 Unix 沒有這回事。
fn repair_mode(path: &Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if std::fs::metadata(path)?.permissions().mode() & 0o7777 != SHIM_MODE {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(SHIM_MODE))?;
            return Ok(true);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(false)
}

/// 掃 `<data_dir>/bots/*/bin`，把已經存在的 shim 換成當前版本。回傳換掉的 `<bot id>/<檔名>`。
///
/// **只補已經有的**：沒有 `bin/` 或沒有那支 shim 的 bot 不生出新檔案——那種 bot（遠端、從沒啟動過）
/// 的 shim 由 `lifecycle::setup::install_shim` 在啟動時處理，這裡不替它決定。
pub fn refresh_all(data_dir: &Path) -> Vec<String> {
    let mut changed = Vec::new();
    let Ok(entries) = std::fs::read_dir(data_dir.join("bots")) else { return changed };
    for entry in entries.flatten() {
        let bot_dir = entry.path();
        if !bot_dir.is_dir() {
            continue;
        }
        let bin = bot_dir.join("bin");
        for (name, content) in shims() {
            let path = bin.join(name);
            if !path.exists() {
                continue;
            }
            match write_atomic(&path, content) {
                Ok(true) => changed.push(format!("{}/{name}", entry.file_name().to_string_lossy())),
                Ok(false) => {}
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "could not refresh this shim"),
            }
        }
    }
    changed
}

/// 開機時跑一次，只記一行 log。
pub fn refresh_at_startup(data_dir: &Path) {
    let changed = refresh_all(data_dir);
    if changed.is_empty() {
        tracing::debug!("every bot shim is already the version this binary carries");
    } else {
        tracing::info!(count = changed.len(), shims = ?changed, "refreshed bot shims to this binary's version (no pane restart needed)");
    }
}

// ───────────────────────────── 遠端（issue #124）─────────────────────────────

/// 遠端一台 host 上的同步結果。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RemoteSync {
    /// 真的換了內容的 `<bot 目錄> <檔名>`。
    pub updated: Vec<String>,
    /// 換版失敗的（舊檔原封不動）。
    pub failed: Vec<String>,
}

/// 把 `names` 那幾支 shim 同步到 `bot_dirs` 每一顆 bot 的 `bin/` 底下的**遠端腳本**（POSIX sh，走 `ssh … sh -s`）。
///
/// **內容來源跟本機開機掃描是同一份**（[`shims`]）——本機與遠端不會各改各的、飄成兩套（issue #124）。行為跟
/// [`write_atomic`] 一致：
/// * 內容一樣就不重寫（`cmp -s`），只把權限確保成 0755——同一台 host 每次重連都會跑，不能把 mtime 洗掉；
/// * 內容不同：暫存檔（同一個目錄）＋ chmod ＋ `mv -f`（rename，原子）。**不能就地截斷**：正在跑的那支 shim
///   沿用舊 inode，下一次 invocation 才拿到新版；
/// * SSH 中途斷線不會留下半支可執行檔：腳本收齊之前 shell 不會執行迴圈（複合命令要整段讀完才跑）；
///   內容先寫進暫存目錄、核對位元組數，才複製；複製失敗就刪暫存檔、舊檔不動。斷線當下已經寫了一半的
///   暫存檔沒有可執行位，而且超過 10 分鐘的殘留（`.<名>.tmp-*`）下一次會被掃掉；
/// * `create_missing = false`（重連時的補版）**只補已經有的**：沒有 `bin/` 或沒有那支 shim 的 bot 不生出新檔案；
///   `true`（bot 啟動時的安裝）才建目錄、建檔。
pub(crate) fn remote_sync_script(bot_dirs: &[String], names: &[&str], create_missing: bool) -> String {
    let mut s = String::new();
    s.push_str("T=$(mktemp -d \"${TMPDIR:-/tmp}/am-shim-sync.XXXXXX\") || { echo AM_SHIM_SYNC_FAILED mktemp; exit 1; }\n");
    s.push_str("trap 'rm -rf \"$T\"' EXIT\n");
    let wanted: Vec<(&str, &str)> = shims().into_iter().filter(|(n, _)| names.contains(n)).collect();
    for (name, content) in &wanted {
        debug_assert!(content.ends_with('\n'), "heredoc 要求結尾換行：{name}");
        s.push_str(&format!("cat > \"$T/{name}\" <<'AM_SHIM_EOF_{name}'\n{content}AM_SHIM_EOF_{name}\n"));
        // 收到的位元組數對不上（傳輸被截斷）就整個放棄，不複製任何東西。
        s.push_str(&format!(
            "[ \"$(wc -c < \"$T/{name}\" | tr -d ' ')\" = {len} ] || {{ echo AM_SHIM_SYNC_FAILED truncated {name}; exit 1; }}\n",
            len = content.len()
        ));
    }
    let dirs = bot_dirs.iter().map(|d| crate::hosts::sh_quote(d)).collect::<Vec<_>>().join(" ");
    let create = if create_missing { "1" } else { "" };
    let list = wanted.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(" ");
    s.push_str(&format!(
        r#"for D in {dirs}; do
    [ -d "$D" ] || {{ [ "{create}" = 1 ] && mkdir -p "$D" || continue; }}
    B="$D/bin"
    [ -d "$B" ] || {{ [ "{create}" = 1 ] && mkdir -p "$B" || continue; }}
    find "$B" -maxdepth 1 -name '.*.tmp-*' -mmin +10 -exec rm -f {{}} + 2>/dev/null
    for name in {list}; do
        f="$B/$name"
        if [ ! -f "$f" ]; then
            [ "{create}" = 1 ] || continue
        elif cmp -s "$T/$name" "$f"; then
            chmod 755 "$f" 2>/dev/null
            continue
        fi
        tmp="$B/.$name.tmp-$$"
        if cp "$T/$name" "$tmp" && chmod 755 "$tmp" && mv -f "$tmp" "$f"; then
            echo "AM_SHIM_UPDATED $D $name"
        else
            rm -f "$tmp"
            echo "AM_SHIM_FAILED $D $name"
        fi
    done
done
echo AM_SHIM_SYNC_DONE
"#
    ));
    s
}

/// 讀 [`remote_sync_script`] 的輸出。沒看到結尾標記（腳本被截斷、ssh 中途斷線）就是失敗——不當成成功。
pub(crate) fn parse_remote_sync(out: &str) -> anyhow::Result<RemoteSync> {
    if let Some(l) = out.lines().find(|l| l.starts_with("AM_SHIM_SYNC_FAILED")) {
        anyhow::bail!("remote shim sync gave up: {l}");
    }
    if !out.lines().any(|l| l == "AM_SHIM_SYNC_DONE") {
        anyhow::bail!("remote shim sync did not confirm:\n{}", out.trim());
    }
    let mut r = RemoteSync::default();
    for l in out.lines() {
        if let Some(rest) = l.strip_prefix("AM_SHIM_UPDATED ") {
            r.updated.push(rest.to_string());
        } else if let Some(rest) = l.strip_prefix("AM_SHIM_FAILED ") {
            r.failed.push(rest.to_string());
        }
    }
    Ok(r)
}

/// 對一台遠端 host 跑 [`remote_sync_script`]。
pub(crate) async fn sync_remote(
    conn: &crate::hosts::HostConn,
    bot_dirs: &[String],
    names: &[&str],
    create_missing: bool,
) -> anyhow::Result<RemoteSync> {
    let out = conn.ssh_exec_timeout(&remote_sync_script(bot_dirs, names, create_missing), std::time::Duration::from_secs(60)).await?;
    parse_remote_sync(&out)
}

/// 連上（或重連）一台遠端 host 之後：盤點這台上還受管理的 bot（`live_bots_on_host`），把它們**已經存在**的
/// `herdr`／`cargo` shim 換成這顆 binary 帶的版本，不必重啟任何 pane。回傳換了什麼。
///
/// 從沒 setup 過的 bot（沒有 `bin/` 或沒有那支 shim）不會被生出新檔案——那是啟動時 `install_shim` 的事。
pub(crate) async fn refresh_remote_host(app: &Arc<App>, host: &str) -> anyhow::Result<RemoteSync> {
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    if conn.is_local() {
        return Ok(RemoteSync::default());
    }
    if !conn.is_connected() {
        anyhow::bail!("host `{host}` is not connected");
    }
    let bots = crate::db::live_bots_on_host(&app.db, host).await?;
    let mut dirs = Vec::new();
    for b in &bots {
        dirs.push(crate::lifecycle::remote_bot_dir_for(&conn, &b.id, app.instance().as_deref()).await?.dir);
    }
    if dirs.is_empty() {
        return Ok(RemoteSync::default());
    }
    let names: Vec<&str> = shims().iter().map(|(n, _)| *n).collect();
    sync_remote(&conn, &dirs, &names, false).await
}

/// host 連上之後在背景補版：**不擋連線、不擋 daemon 啟動**。失敗（ssh 抖了、host 剛好又掉線）就重試兩次
/// （20 秒、60 秒後）；host 那時已不在線就放棄——下一次 supervisor 連上會再叫一次（補版是冪等的）。
pub(crate) fn spawn_remote_refresh(app: Arc<App>, host: String) {
    tokio::spawn(async move {
        for wait in [0u64, 20, 60] {
            if wait > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            }
            match app.hosts.get(&host).await {
                Some(c) if c.is_connected() => {}
                _ => return,
            }
            match refresh_remote_host(&app, &host).await {
                Ok(r) if r.updated.is_empty() && r.failed.is_empty() => {
                    tracing::debug!(host = %host, "every remote bot shim is already the version this binary carries");
                    return;
                }
                Ok(r) => {
                    if !r.updated.is_empty() {
                        tracing::info!(host = %host, count = r.updated.len(), shims = ?r.updated, "refreshed remote bot shims to this binary's version (no pane restart needed)");
                    }
                    if !r.failed.is_empty() {
                        tracing::warn!(host = %host, shims = ?r.failed, "could not refresh some remote shims (old files untouched)");
                    }
                    return;
                }
                Err(e) => tracing::warn!(host = %host, attempt = wait, error = %e, "could not refresh remote bot shims; will retry"),
            }
        }
    });
}

/// `<data_dir>/bots/<id>/bin` 的路徑，給 `install_local` 共用。
pub fn bin_dir(bot_dir: &Path) -> PathBuf {
    bot_dir.join("bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("am-shim-refresh-{}", crate::db::ulid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seed(data_dir: &Path, bot: &str, body: &str) -> PathBuf {
        let bin = data_dir.join("bots").join(bot).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["herdr", "cargo"] {
            std::fs::write(bin.join(name), body).unwrap();
        }
        bin
    }

    #[cfg(unix)]
    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    fn set_mode(p: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// 內容就是這顆 binary 帶的版本、權限也是 0755——一顆健康的 shim。
    fn seed_current(data_dir: &Path, bot: &str) -> PathBuf {
        let bin = data_dir.join("bots").join(bot).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for (name, content) in shims() {
            std::fs::write(bin.join(name), content).unwrap();
            #[cfg(unix)]
            set_mode(&bin.join(name), 0o755);
        }
        bin
    }

    /// 2026-09-18：新版上線了，長跑 bot 手上還是舊 shim（沒有 `AM_SHIM_MARKER`），照樣巢狀死鎖。
    /// 開機掃描要把它換掉，不必等那顆 bot 重啟 pane。
    #[test]
    fn an_old_shim_is_replaced_without_touching_the_pane() {
        let data = tmp();
        let bin = seed(&data, "01OLD", "#!/bin/sh\n# 舊版，沒有 marker\nexec cargo \"$@\"\n");
        let changed = refresh_all(&data);
        assert_eq!(changed.len(), 2, "{changed:?}");
        assert_eq!(std::fs::read_to_string(bin.join("cargo")).unwrap(), crate::cargo_shim::SHIM_SH);
        assert_eq!(std::fs::read_to_string(bin.join("herdr")).unwrap(), crate::herdr_shim::SHIM_SH);
        assert!(std::fs::read_to_string(bin.join("cargo")).unwrap().contains("AM_SHIM_MARKER"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(bin.join("cargo")).unwrap().permissions().mode() & 0o777, 0o755);
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 內容一樣、權限也對：真的什麼都不做——mtime 不動，連 chmod 都不呼叫（ctime 也不動）。
    /// 每次重啟都重寫會把 mtime 洗掉，之後沒人分得出哪些 shim 真的換過版。
    #[cfg(unix)]
    #[test]
    fn a_shim_that_is_already_current_is_left_alone() {
        let data = tmp();
        let bin = seed_current(&data, "01NEW");
        let stat = |n: &str| {
            use std::os::unix::fs::MetadataExt as _;
            let m = std::fs::metadata(bin.join(n)).unwrap();
            (m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec(), m.ino())
        };
        let before = (stat("cargo"), stat("herdr"));

        assert!(refresh_all(&data).is_empty());
        assert_eq!((stat("cargo"), stat("herdr")), before, "mtime／ctime／inode 都不能動");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// issue #126：內容已經是現行版、但**權限掉了**（舊版 `install_local` 寫完才 chmod，中間死掉就留下 0644）。
    /// 只比內容會當成「什麼都不用做」，pane 下一次打 `cargo` 就 permission denied——所以要把權限修回 0755，
    /// 而且**只 chmod**：內容沒變，不重寫（mtime、inode 都不動）。cargo／herdr 兩支都要。
    #[cfg(unix)]
    #[test]
    fn a_current_shim_that_lost_its_exec_bit_is_made_executable_again() {
        use std::os::unix::fs::MetadataExt as _;
        for broken in [0o644, 0o600, 0o664, 0o444] {
            let data = tmp();
            let bin = seed_current(&data, "01MODE");
            for (name, _) in shims() {
                set_mode(&bin.join(name), broken);
            }
            let before: Vec<_> = shims()
                .iter()
                .map(|(n, _)| {
                    let m = std::fs::metadata(bin.join(n)).unwrap();
                    (m.mtime(), m.mtime_nsec(), m.ino())
                })
                .collect();

            let mut changed = refresh_all(&data);
            changed.sort();
            assert_eq!(changed, vec!["01MODE/cargo".to_string(), "01MODE/herdr".to_string()], "{broken:o} 要算修過：{changed:?}");
            for (i, (name, content)) in shims().iter().enumerate() {
                let p = bin.join(name);
                assert_eq!(mode_of(&p), 0o755, "{name} 從 {broken:o} 要修回 0755");
                assert_eq!(&std::fs::read_to_string(&p).unwrap(), content, "{name} 內容不該被動");
                let m = std::fs::metadata(&p).unwrap();
                assert_eq!((m.mtime(), m.mtime_nsec(), m.ino()), before[i], "{name}：只 chmod，不重寫內容（mtime／inode 不動）");
            }
            // 修完之後再跑就是真的 no-op（冪等）。
            assert!(refresh_all(&data).is_empty());
            let _ = std::fs::remove_dir_all(&data);
        }
    }

    /// 直接呼叫 `write_atomic`（`install_local` 走的那條）：權限不對也要修，回 `Ok(true)`；健康的回 `Ok(false)`。
    #[cfg(unix)]
    #[test]
    fn write_atomic_reports_a_mode_repair_and_nothing_for_a_healthy_file() {
        let data = tmp();
        let p = data.join("cargo");
        assert!(write_atomic(&p, "#!/bin/sh\n:\n").unwrap(), "新檔案");
        assert_eq!(mode_of(&p), 0o755);
        assert!(!write_atomic(&p, "#!/bin/sh\n:\n").unwrap(), "健康：什麼都不做");
        set_mode(&p, 0o644);
        assert!(write_atomic(&p, "#!/bin/sh\n:\n").unwrap(), "同內容、掉了權限：修好了");
        assert_eq!(mode_of(&p), 0o755);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 內容不同：仍是暫存檔 + rename，**不是**就地截斷。舊 inode（正在跑那支 shim 的行程手上還開著）
    /// 內容原封不動，只有目錄項換掉；新的一份權限是 0755。
    #[cfg(unix)]
    #[test]
    fn a_different_shim_is_swapped_by_rename_and_the_old_inode_is_untouched() {
        use std::io::Read as _;
        let data = tmp();
        let bin = seed(&data, "01SWAP", "#!/bin/sh\n# 舊版\n");
        let mut old_handle = std::fs::File::open(bin.join("cargo")).unwrap();

        assert_eq!(refresh_all(&data).len(), 2);

        let mut seen_by_old_process = String::new();
        old_handle.read_to_string(&mut seen_by_old_process).unwrap();
        assert_eq!(seen_by_old_process, "#!/bin/sh\n# 舊版\n", "就地截斷會讓正在跑的 shim 讀到新內容或空檔案");
        assert_eq!(std::fs::read_to_string(bin.join("cargo")).unwrap(), crate::cargo_shim::SHIM_SH);
        assert_eq!(mode_of(&bin.join("cargo")), 0o755);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 只補**已經有的**：沒有 bin／沒有那支 shim 的 bot 不生出新檔案，其他檔案也不碰。
    #[test]
    fn nothing_is_created_for_bots_that_never_had_a_shim() {
        let data = tmp();
        let bare = data.join("bots").join("01BARE");
        std::fs::create_dir_all(&bare).unwrap();
        let bin = data.join("bots").join("01HALF").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("cargo"), "#!/bin/sh\n# 舊\n").unwrap();
        std::fs::write(bin.join("something-else"), "keep me").unwrap();

        let changed = refresh_all(&data);
        assert_eq!(changed, vec!["01HALF/cargo".to_string()], "{changed:?}");
        assert!(!bare.join("bin").exists(), "沒有 bin 的 bot 不該被生出目錄");
        assert!(!bin.join("herdr").exists(), "本來就沒有的 shim 不補");
        assert_eq!(std::fs::read_to_string(bin.join("something-else")).unwrap(), "keep me");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 換版是 rename：暫存檔不留下來。
    #[test]
    fn the_swap_leaves_no_temporary_file_behind() {
        let data = tmp();
        let bin = seed(&data, "01TMP", "#!/bin/sh\n# 舊\n");
        refresh_all(&data);
        let leftovers: Vec<String> = std::fs::read_dir(&bin)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&data);
    }

    // ───────────── 遠端（issue #124）：腳本直接在本機的假「遠端」目錄上跑，不需要真的 ssh ─────────────

    /// 用 `sh -s` 跑 [`remote_sync_script`]（跟 `HostConn::ssh_exec` 送腳本的方式一樣：走 stdin）。
    #[cfg(unix)]
    fn run_script(script: &str, path: Option<&str>) -> String {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new("sh");
        cmd.arg("-s").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        if let Some(p) = path {
            cmd.env("PATH", p);
        }
        let mut child = cmd.spawn().unwrap();
        // 收不完（腳本被截斷）時 shell 可能先結束，寫入端會 EPIPE——那正是要模擬的情況，不算錯。
        let _ = child.stdin.take().unwrap().write_all(script.as_bytes());
        String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned()
    }

    #[cfg(unix)]
    fn run_sync(dirs: &[&Path], names: &[&str], create_missing: bool) -> String {
        let dirs: Vec<String> = dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect();
        run_script(&remote_sync_script(&dirs, names, create_missing), None)
    }

    #[cfg(unix)]
    fn write_shim(path: &Path, body: &str, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        set_mode(path, mode);
    }

    #[cfg(unix)]
    fn touch_old(path: &Path) {
        assert!(std::process::Command::new("touch").args(["-t", "202601010000"]).arg(path).status().unwrap().success());
    }

    /// heredoc 要求內容以換行結尾，不然結尾標記黏在最後一行上、腳本就壞了。
    #[test]
    fn every_shim_ends_with_a_newline() {
        for (name, content) in shims() {
            assert!(content.ends_with('\n'), "{name}");
        }
    }

    /// issue #124：daemon 升級後，長跑的**遠端** bot 手上也還是舊 shim。重連之後盤點這台的 bot，把**已經存在**的
    /// shim 換成這顆 binary 帶的版本：暫存檔＋chmod＋rename（正在跑的舊 shim 沿用舊 inode）、內容一樣不重寫
    /// （不洗 mtime，只確保權限）、沒有 bin／沒有那支 shim 的 bot 不生出新檔案、上次斷線留下的殘留暫存檔掃掉、
    /// 重跑是冪等的。
    #[cfg(unix)]
    #[test]
    fn a_remote_sync_replaces_old_shims_atomically_and_only_where_they_already_exist() {
        use std::io::Read as _;
        let d = tmp();
        let (b1, b2, b3, b4) = (d.join("b1"), d.join("b2"), d.join("b3"), d.join("b4"));
        write_shim(&b1.join("bin/herdr"), "#!/bin/sh\n# 舊版 herdr\n", 0o755); // 舊內容
        write_shim(&b1.join("bin/cargo"), crate::cargo_shim::SHIM_SH, 0o644); // 內容對、權限掉了
        write_shim(&b2.join("bin/herdr"), crate::herdr_shim::SHIM_SH, 0o755); // 健康的現行版
        touch_old(&b2.join("bin/herdr"));
        // b2 沒有 cargo shim（從沒裝過）、b3 只有 bot 目錄沒有 bin、b4 根本不存在。
        std::fs::create_dir_all(&b3).unwrap();
        write_shim(&b1.join("bin/.herdr.tmp-999"), "殘留的半截暫存檔", 0o644);
        touch_old(&b1.join("bin/.herdr.tmp-999"));
        let mtime = |p: &Path| std::fs::metadata(p).unwrap().modified().unwrap();
        let b2_mtime = mtime(&b2.join("bin/herdr"));
        let mut running_old = std::fs::File::open(b1.join("bin/herdr")).unwrap(); // 「正在執行」的舊 shim

        let out = run_sync(&[&b1, &b2, &b3, &b4], &["herdr", "cargo"], false);
        let r = parse_remote_sync(&out).unwrap();

        assert_eq!(r.updated, vec![format!("{} herdr", b1.display())], "只有內容真的不同的那一支：{out}");
        assert!(r.failed.is_empty(), "{out}");
        assert_eq!(std::fs::read_to_string(b1.join("bin/herdr")).unwrap(), crate::herdr_shim::SHIM_SH);
        assert_eq!(mode_of(&b1.join("bin/herdr")), 0o755);
        assert_eq!(mode_of(&b1.join("bin/cargo")), 0o755, "內容對、權限掉了：只 chmod");
        assert_eq!(std::fs::read_to_string(b1.join("bin/cargo")).unwrap(), crate::cargo_shim::SHIM_SH);
        let mut seen_by_old = String::new();
        running_old.read_to_string(&mut seen_by_old).unwrap();
        assert_eq!(seen_by_old, "#!/bin/sh\n# 舊版 herdr\n", "正在跑的舊 shim 沿用舊 inode，不能被就地截斷");
        assert_eq!(mtime(&b2.join("bin/herdr")), b2_mtime, "同內容不重寫：mtime 不能被洗掉");
        assert!(!b2.join("bin/cargo").exists(), "從沒裝過的 shim 不補");
        assert!(!b3.join("bin").exists(), "沒有 bin 的 bot 不生出目錄");
        assert!(!b4.exists(), "不存在的 bot 目錄不建立");
        let leftovers: Vec<String> = std::fs::read_dir(b1.join("bin")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with('.')).collect();
        assert!(leftovers.is_empty(), "殘留的暫存檔要掃掉：{leftovers:?}");

        // 冪等：再跑一次什麼都沒換。
        let again = parse_remote_sync(&run_sync(&[&b1, &b2, &b3, &b4], &["herdr", "cargo"], false)).unwrap();
        assert_eq!(again, RemoteSync::default());
        assert_eq!(mtime(&b2.join("bin/herdr")), b2_mtime);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 本機開機掃描與遠端同步讀的是**同一份**內容來源：同樣的舊狀態，兩邊跑完的檔案位元組與權限完全一樣。
    #[cfg(unix)]
    #[test]
    fn the_remote_sync_and_the_local_refresh_produce_the_same_files() {
        let d = tmp();
        let local = d.join("local");
        let remote = d.join("remote/bot");
        for bin in [local.join("bots/B/bin"), remote.join("bin")] {
            write_shim(&bin.join("herdr"), "#!/bin/sh\n# 舊\n", 0o755);
            write_shim(&bin.join("cargo"), crate::cargo_shim::SHIM_SH, 0o600);
        }
        refresh_all(&local);
        run_sync(&[&remote], &["herdr", "cargo"], false);
        for (name, content) in shims() {
            let l = local.join("bots/B/bin").join(name);
            let r = remote.join("bin").join(name);
            assert_eq!(std::fs::read(&l).unwrap(), std::fs::read(&r).unwrap(), "{name}");
            assert_eq!(std::fs::read_to_string(&r).unwrap(), content, "{name}");
            assert_eq!((mode_of(&l), mode_of(&r)), (0o755, 0o755), "{name}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 複製到一半失敗（磁碟滿、被砍）：舊檔原封不動、沒有留下半截的檔案，而且明說這一支失敗。
    #[cfg(unix)]
    #[test]
    fn a_copy_that_fails_midway_leaves_the_old_shim_untouched_and_no_partial_file() {
        let d = tmp();
        let bot = d.join("bot");
        write_shim(&bot.join("bin/herdr"), "#!/bin/sh\n# 舊版\n", 0o755);
        // 一支會寫出 10 個位元組就死掉的 `cp`，蓋在 PATH 最前面。
        let fake = d.join("fakebin");
        std::fs::create_dir_all(&fake).unwrap();
        write_shim(&fake.join("cp"), "#!/bin/sh\nhead -c 10 \"$1\" > \"$2\"\nexit 1\n", 0o755);

        let script = remote_sync_script(&[bot.to_string_lossy().into_owned()], &["herdr"], false);
        let out = run_script(&script, Some(&format!("{}:/usr/bin:/bin", fake.display())));
        let r = parse_remote_sync(&out).unwrap();

        assert_eq!(r.failed, vec![format!("{} herdr", bot.display())], "{out}");
        assert!(r.updated.is_empty());
        assert_eq!(std::fs::read_to_string(bot.join("bin/herdr")).unwrap(), "#!/bin/sh\n# 舊版\n", "舊檔不動");
        assert_eq!(mode_of(&bot.join("bin/herdr")), 0o755);
        let names: Vec<String> = std::fs::read_dir(bot.join("bin")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["herdr".to_string()], "沒有留下半截的暫存檔：{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// SSH 送到一半斷線：shell 收到的是被截斷的腳本。不管斷在哪——第一支 heredoc 中間、迴圈之前、迴圈中間——
    /// 都不能動任何 shim，也不能留下暫存檔；而且輸出沒有結尾標記，呼叫端不會把它當成功。
    #[cfg(unix)]
    #[test]
    fn a_script_cut_off_by_a_dropped_ssh_connection_changes_nothing() {
        let d = tmp();
        let bot = d.join("bot");
        write_shim(&bot.join("bin/herdr"), "#!/bin/sh\n# 舊版\n", 0o755);
        let script = remote_sync_script(&[bot.to_string_lossy().into_owned()], &["herdr", "cargo"], false);
        let loop_at = script.find("for D in").unwrap();
        let loop_end = loop_at + script[loop_at..].find("\ndone\necho").unwrap();
        let mut cuts = vec![loop_at / 2, loop_at - 5, loop_at + 30, loop_end - 3];
        cuts.iter_mut().for_each(|c| {
            while !script.is_char_boundary(*c) {
                *c -= 1;
            }
        });
        for cut in cuts {
            let out = run_script(&script[..cut], None);
            assert!(parse_remote_sync(&out).is_err(), "沒收完就不是成功（cut={cut}）：{out}");
            assert_eq!(std::fs::read_to_string(bot.join("bin/herdr")).unwrap(), "#!/bin/sh\n# 舊版\n", "cut={cut}");
            let names: Vec<String> = std::fs::read_dir(bot.join("bin")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            assert_eq!(names, vec!["herdr".to_string()], "cut={cut}：{names:?}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// bot 啟動時的安裝（`create_missing`）才會建目錄、建檔，而且只建被點名的那支；補版（重連）不建。
    #[cfg(unix)]
    #[test]
    fn a_remote_sync_creates_a_missing_shim_only_when_asked_to() {
        let d = tmp();
        let bot = d.join("bot");
        std::fs::create_dir_all(&bot).unwrap();
        run_sync(&[&bot], &["herdr", "cargo"], false);
        assert!(!bot.join("bin").exists(), "補版不生新檔案");

        let r = parse_remote_sync(&run_sync(&[&bot], &["herdr"], true)).unwrap();
        assert_eq!(r.updated, vec![format!("{} herdr", bot.display())]);
        assert_eq!(std::fs::read_to_string(bot.join("bin/herdr")).unwrap(), crate::herdr_shim::SHIM_SH);
        assert_eq!(mode_of(&bot.join("bin/herdr")), 0o755);
        assert!(!bot.join("bin/cargo").exists(), "只裝被點名的那支");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 沒看到結尾標記就不是成功；腳本自己放棄（暫存目錄建不起來、收到的位元組數不對）也是失敗。
    #[test]
    fn a_remote_sync_that_did_not_reach_its_end_is_not_a_success() {
        assert!(parse_remote_sync("").is_err());
        assert!(parse_remote_sync("AM_SHIM_UPDATED /b herdr\n").is_err(), "有換版但沒收尾：不確定，當失敗");
        assert!(parse_remote_sync("AM_SHIM_SYNC_FAILED truncated herdr\n").is_err());
        assert_eq!(parse_remote_sync("AM_SHIM_UPDATED /b herdr\nAM_SHIM_FAILED /c cargo\nAM_SHIM_SYNC_DONE\n").unwrap(), RemoteSync { updated: vec!["/b herdr".into()], failed: vec!["/c cargo".into()] });
    }
}
