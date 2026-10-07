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

use std::path::{Path, PathBuf};

/// 一顆 bot 的 bin 目錄裡，我們負責的檔案與它現在該有的內容。
///
/// **本機開機掃描（[`refresh_all`]）與遠端同步（[`remote_sync_script`]）讀的是同一份**——兩邊不會各改各的、
/// 飄成兩套 shim（issue #124）。
pub fn shims() -> [(&'static str, &'static str); 2] {
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
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("shim");
    let tmp = dir.join(format!(".{name}.tmp-{}", ulid::Ulid::new()));
    let write_result = (|| {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(content.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(SHIM_MODE))?;
        }
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(true)
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

/// 一次本機掃描的結果。`failed` 跟 `changed` 一樣重要：換不動的 shim 不會自己好，
/// 而它就是「這顆 bot 的 cargo 還是舊的」——2026-09-18 的巢狀死鎖與 #417 都是這樣來的。
#[derive(Debug, Default)]
pub struct LocalRefresh {
    /// 真的動了的 `<bot id>/<檔名>`。
    pub changed: Vec<String>,
    /// 換不動的 `(<bot id>/<檔名>, 錯誤)`。
    pub failed: Vec<(String, String)>,
}

/// 掃 `<data_dir>/bots/*/bin`，把已經存在的 shim 換成當前版本。
///
/// **只補已經有的**：沒有 `bin/` 或沒有那支 shim 的 bot 不生出新檔案——那種 bot（遠端、從沒啟動過）
/// 的 shim 由 `lifecycle::setup::install_shim` 在啟動時處理，這裡不替它決定。
pub fn refresh_all(data_dir: &Path) -> LocalRefresh {
    let mut out = LocalRefresh::default();
    let Ok(entries) = std::fs::read_dir(data_dir.join("bots")) else { return out };
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
            let who = format!("{}/{name}", entry.file_name().to_string_lossy());
            match write_atomic(&path, content) {
                Ok(true) => out.changed.push(who),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "could not refresh this shim");
                    out.failed.push((who, e.to_string()));
                }
            }
        }
    }
    out
}

/// 這顆 bot 的 shim 換不動時的 inbox 種類（issue #533）。
///
/// 跟 `cli_refresh` 的 `agm_cli_stale` 同一個形狀、同一個理由：**換不動的檔案不會自己好**，
/// 而下一次修正機會要等下一次 daemon 重啟。只記 `tracing::warn!` 等於沒人會知道——沒有人固定
/// 看 daemon.log，而舊 shim 的後果（cargo 巢狀死鎖、工作沒被轉到外部編譯主機）在畫面上看起來
/// 只是「這顆 bot 很慢」。
pub const SHIM_STALE_KIND: &str = "bot_shim_stale";

pub const REMOTE_RETRY_WAITS: [u64; 4] = [0, 300, 900, 3600];

// ───────────────────────────── 遠端（issue #124）─────────────────────────────

/// 遠端一台 host 上的同步結果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RemoteSync {
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
pub fn remote_sync_script(bot_dirs: &[String], names: &[&str], create_missing: bool) -> String {
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
        tmp=$(mktemp "$B/.$name.tmp-XXXXXX") || {{ echo "AM_SHIM_FAILED $D $name"; continue; }}
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
pub fn parse_remote_sync(out: &str) -> anyhow::Result<RemoteSync> {
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
pub async fn sync_remote(
    conn: &crate::hosts::HostConn,
    bot_dirs: &[String],
    names: &[&str],
    create_missing: bool,
) -> anyhow::Result<RemoteSync> {
    let out = conn.ssh_exec_timeout(&remote_sync_script(bot_dirs, names, create_missing), std::time::Duration::from_secs(60)).await?;
    parse_remote_sync(&out)
}

/// `<data_dir>/bots/<id>/bin` 的路徑，給 `install_local` 共用。
pub fn bin_dir(bot_dir: &Path) -> PathBuf {
    bot_dir.join("bin")
}



/// 遠端 shim 過期的主機帳。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait RemoteShimStale: Send + Sync {
    fn remote_shim_stale(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, String>>;
}
