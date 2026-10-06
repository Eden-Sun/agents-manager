//! agy（Antigravity CLI）安裝／更新到任一台主機（SPEC §12a.11、API `POST /api/hosts/{name}/agy/install`）。
//!
//! agy 沒有 npm／brew 套件，官方 `install.sh` 最後一步會跑 `agy install`（改 shell profile、purge alias），所以**絕不執行它**：
//! daemon 自己讀官方 manifest（`<MANIFEST_BASE>/<platform>.json` ＝ `{version, url, sha512}`），驗過 url 與 sha512 的形狀後，
//! 把它們當**資料**交給主機端一段寫死的 POSIX sh——下載 tarball、驗 sha512、只解出 `antigravity` 一個檔、跑 `--version` 確認版本
//! （`AGY_CLI_DISABLE_AUTO_UPDATE=true`）、同目錄複製後 `mv` 蓋過 `~/.local/bin/agy`。任何一步失敗都不碰既有的 agy。
//!
//! 刻意的邊界：
//! - 只給使用者按（帶 bot 身分的請求 403，同 `agy/logout`）；指令不接受呼叫端傳入，要裝哪一版只看官方 manifest。
//! - 同一台同時只裝一次：行程內一個集合擋同實例，主機端另有安裝鎖（[`crate::cli_update`] 那套 process-group 鎖、換一條鎖檔）擋隔離實例與重啟後殘留的安裝。
//! - 裝完重新偵測那台的 CLI（`tools::detect`）：偵測到的版本要等於 manifest 版本，否則回錯（常見原因是 `~/.local/bin` 不在那台的 PATH）。
//! - 登入不在這裡：agy 沒有 `login` 子命令，由使用者在 host shell 跑 `agy`（額度欄「開 shell 登入」），憑證檔出現後 [`crate::quota_agy::spawn_agy_login_watcher`] 翻成已登入。
//! - 會動到機器的兩件事（在主機跑腳本、讀 manifest）走 [`Env`]，測試換成假的；腳本本身另有真跑 `/bin/sh` 的測試（假 HOME、`file://` 來源）。

use crate::hosts::HostFence;
use crate::state::App;
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// 官方 manifest 的位置（SPEC 附錄 G.1）。
pub const MANIFEST_BASE: &str = "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests";
/// tarball 只收官方的儲存桶：manifest 被換掉也不能叫主機去抓別處。
const DOWNLOAD_PREFIX: &str = "https://storage.googleapis.com/antigravity-public/antigravity-cli/";
/// 主機端安裝鎖（見 [`crate::cli_update`] 的 `locked_script`）。
pub const INSTALL_LOCK: &str = "$HOME/.agents-manager-agy-install.lock";
/// 下載 50 MB、解開 175 MB；網路慢給到五分鐘，再久就是卡住了。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(20);
const TAIL_CHARS: usize = 1500;

/// 官方 manifest 驗過形狀後的內容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    pub url: String,
    pub sha512: String,
}

/// url 與版本會被拼進主機端的 shell（單引號內）：只收明確安全的字元，別的一律當 manifest 壞掉。
fn safe_token(s: &str, extra: &[char]) -> bool {
    !s.is_empty() && s.len() <= 300 && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

pub fn parse_manifest(raw: &str) -> Result<Manifest, String> {
    let v: Value = serde_json::from_str(raw.trim()).map_err(|e| format!("manifest 不是 JSON：{e}"))?;
    let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| format!("manifest 缺 `{k}`"));
    let (version, url, sha) = (field("version")?, field("url")?, field("sha512")?);
    if !safe_token(version, &['.', '-']) || !version.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(format!("manifest 的 version 看不懂：{version}"));
    }
    if !url.starts_with(DOWNLOAD_PREFIX) || !safe_token(url, &[':', '/', '.', '_', '-', '~']) {
        return Err(format!("manifest 的 url 不在官方儲存桶：{url}"));
    }
    if sha.len() != 128 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("manifest 的 sha512 不是 128 位十六進位".into());
    }
    Ok(Manifest { version: version.into(), url: url.into(), sha512: sha.to_ascii_lowercase() })
}

/// 主機的作業系統與 libc：兩行 `AM_UNAME <s> <m>`、`AM_MUSL 0|1`。
pub const PLATFORM_PROBE_SH: &str = r#"s=$(uname -s 2>/dev/null); m=$(uname -m 2>/dev/null)
printf 'AM_UNAME %s %s\n' "$s" "$m"
musl=0
if [ "$s" = Linux ]; then
  if ldd --version 2>&1 | grep -qi musl || ls /lib/ld-musl-* >/dev/null 2>&1; then musl=1; fi
fi
printf 'AM_MUSL %s\n' "$musl"
"#;

/// manifest 的 `<platform>`（SPEC 附錄 G.1）。沒有對應就是不支援。
pub fn platform_of(probe_out: &str) -> Option<&'static str> {
    let mut uname = None;
    let mut musl = false;
    for line in probe_out.lines() {
        if let Some(rest) = line.trim().strip_prefix("AM_UNAME ") {
            let mut it = rest.split_whitespace();
            uname = Some((it.next()?.to_string(), it.next()?.to_string()));
        } else if line.trim() == "AM_MUSL 1" {
            musl = true;
        }
    }
    let (os, arch) = uname?;
    match (os.as_str(), arch.as_str(), musl) {
        ("Darwin", "arm64", _) => Some("darwin_arm64"),
        ("Darwin", "x86_64", _) => Some("darwin_amd64"),
        ("Linux", "x86_64" | "amd64", false) => Some("linux_amd64"),
        ("Linux", "x86_64" | "amd64", true) => Some("linux_amd64_musl"),
        ("Linux", "aarch64" | "arm64", false) => Some("linux_arm64"),
        ("Linux", "aarch64" | "arm64", true) => Some("linux_arm64_musl"),
        _ => None,
    }
}

/// 主機端安裝本體（POSIX sh，由 [`crate::cli_update::locked_script`] 包進安裝鎖後執行）。
///
/// 順序是刻意的：下載 → 驗 sha512 → 才解開 → 對**暫存檔**跑 `--version`（版本要吻合）→ 同目錄複製 → `mv` 取代。
/// 驗證沒過就不會碰既有的 `~/.local/bin/agy`；沒驗過 sha512 的東西不會被執行。成功印 `AM_AGY_INSTALLED <路徑> <版本>`。
pub fn install_body(m: &Manifest) -> String {
    format!(
        r#"set -eu
URL={url}
SHA={sha}
WANT={version}
BIN="$HOME/.local/bin"
mkdir -p "$BIN"
T=$(mktemp -d "${{TMPDIR:-/tmp}}/am-agy-install.XXXXXX") || exit 70
trap 'rm -rf "$T"' EXIT
curl -fsSL --retry 2 --connect-timeout 20 -o "$T/agy.tgz" "$URL" || {{ echo "AM_AGY_DOWNLOAD_FAILED $URL" >&2; exit 71; }}
if command -v sha512sum >/dev/null 2>&1; then GOT=$(sha512sum "$T/agy.tgz" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then GOT=$(shasum -a 512 "$T/agy.tgz" | cut -d' ' -f1)
elif command -v openssl >/dev/null 2>&1; then GOT=$(openssl dgst -sha512 "$T/agy.tgz" | sed 's/^.*= //')
else echo "AM_AGY_NO_SHA512_TOOL" >&2; exit 72; fi
[ "$GOT" = "$SHA" ] || {{ echo "AM_AGY_SHA512_MISMATCH want=$SHA got=$GOT" >&2; exit 73; }}
mkdir "$T/x"
tar -xzf "$T/agy.tgz" -C "$T/x" antigravity || {{ echo "AM_AGY_EXTRACT_FAILED" >&2; exit 74; }}
[ -f "$T/x/antigravity" ] && [ ! -L "$T/x/antigravity" ] || {{ echo "AM_AGY_EXTRACT_FAILED not a regular file" >&2; exit 74; }}
chmod 755 "$T/x/antigravity"
GOTV=$(AGY_CLI_DISABLE_AUTO_UPDATE=true "$T/x/antigravity" --version </dev/null 2>&1 | head -1 | tr -d '\r') || GOTV=""
case "$GOTV" in *"$WANT"*) ;; *) echo "AM_AGY_VERSION_MISMATCH want=$WANT got=$GOTV" >&2; exit 75;; esac
cp "$T/x/antigravity" "$BIN/.agy.new.$$"
chmod 755 "$BIN/.agy.new.$$"
mv -f "$BIN/.agy.new.$$" "$BIN/agy"
printf 'AM_AGY_INSTALLED %s %s\n' "$BIN/agy" "$GOTV"
"#,
        url = crate::hosts::sh_quote(&m.url),
        sha = crate::hosts::sh_quote(&m.sha512),
        version = crate::hosts::sh_quote(&m.version),
    )
}

#[derive(Debug)]
pub enum InstallError {
    UnknownHost,
    /// 這台已經有一個 agy 安裝在跑（本行程或主機端的鎖）；沒有跑任何安裝指令。
    Busy(String),
    Failed { reason: &'static str, message: String },
}

fn failed(reason: &'static str, message: impl Into<String>) -> InstallError {
    InstallError::Failed { reason, message: message.into() }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Installed {
    pub host: String,
    pub platform: String,
    pub from: Option<String>,
    pub to: String,
    pub path: Option<String>,
    /// 官方最新版本就是那台現在的版本：沒下載、沒動檔案。
    pub already_latest: bool,
}

/// 會動到機器的兩件事。正式版是 [`Real`]；測試換成假的。
pub trait Env: Send + Sync {
    /// 在主機上跑一段 sh 腳本，回 stdout；失敗是帶 stderr 尾巴的訊息。
    fn run<'a>(&'a self, fence: &'a HostFence, script: &'a str, timeout: Duration) -> BoxFuture<'a, Result<String, String>>;
    /// 官方 manifest 的原文。
    fn manifest<'a>(&'a self, platform: &'a str) -> BoxFuture<'a, Result<String, String>>;
}

pub struct Real;

fn tail(s: &str) -> String {
    let t = s.trim();
    let n = t.chars().count();
    if n <= TAIL_CHARS { t.to_string() } else { format!("…{}", t.chars().skip(n - TAIL_CHARS).collect::<String>()) }
}

impl Env for Real {
    fn run<'a>(&'a self, fence: &'a HostFence, script: &'a str, timeout: Duration) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            if fence.conn().is_local() {
                // 本機：整個 process group 一起收（逾時也不留 curl／tar）；stderr 併進 stdout 才看得到失敗原因。
                let wrapped = format!("exec 2>&1\n{script}");
                let out = crate::hosts::sh_local(&wrapped, timeout)
                    .await
                    .map_err(|e| format!("{e:#}"))?
                    .ok_or_else(|| format!("超過 {} 秒沒結束，已中止", timeout.as_secs()))?;
                let text = String::from_utf8_lossy(&out.stdout).to_string();
                if out.status.success() { Ok(text) } else { Err(format!("{}：{}", out.status, tail(&text))) }
            } else {
                // 遠端逾時只砍得掉本機這條 ssh；那邊的安裝可能還在跑（鎖也還在它手上）。
                fence.conn().ssh_exec_path_timeout(script, timeout).await.map_err(|e| tail(&format!("{e:#}")))
            }
        })
    }

    fn manifest<'a>(&'a self, platform: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let client = reqwest::Client::builder().timeout(MANIFEST_TIMEOUT).build().map_err(|e| e.to_string())?;
            let res = client.get(format!("{MANIFEST_BASE}/{platform}.json")).send().await.map_err(|e| format!("讀不到官方 manifest：{e}"))?;
            if !res.status().is_success() {
                return Err(format!("官方 manifest 回 {}", res.status()));
            }
            res.text().await.map_err(|e| format!("讀官方 manifest 失敗：{e}"))
        })
    }
}

fn running() -> &'static Mutex<HashSet<String>> {
    static R: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// 同一台只准一個安裝在跑；離開時放掉（含 panic）。
struct Slot(String);
impl Slot {
    fn take(host: &str) -> Option<Self> {
        running().lock().unwrap_or_else(|e| e.into_inner()).insert(host.to_string()).then(|| Slot(host.to_string()))
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        running().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// `agy --version` 的輸出（可能只有版本號、也可能帶前綴）裡有沒有恰好這一版——不是子字串，`1.3.0` 不該吻合 `11.3.01`。
fn has_version(output: &str, want: &str) -> bool {
    output.split_whitespace().any(|t| t.trim_start_matches('v') == want)
}

async fn cached_version(app: &Arc<App>, host: &str) -> Option<String> {
    app.tools.lock().await.get(host).and_then(|h| h.tools.get("agy")).filter(|t| t.installed).and_then(|t| t.version.clone())
}

fn installed_marker(out: &str) -> Option<(String, String)> {
    let rest = out.lines().find_map(|l| l.trim().strip_prefix("AM_AGY_INSTALLED "))?;
    let (path, version) = rest.split_once(' ').unwrap_or((rest, ""));
    Some((path.to_string(), version.trim().to_string()))
}

/// `POST /api/hosts/{name}/agy/install`：裝／更新那台主機的 agy 到官方最新版。
pub async fn install(app: &Arc<App>, host: &str) -> Result<Installed, InstallError> {
    install_with(app, host, &Real).await
}

pub async fn install_with(app: &Arc<App>, host: &str, env: &dyn Env) -> Result<Installed, InstallError> {
    let fence = app.hosts.fence(host).await.ok_or(InstallError::UnknownHost)?;
    let Some(_slot) = Slot::take(host) else {
        return Err(InstallError::Busy(format!("{host} 已經在安裝 agy，這一下沒有再開一次")));
    };
    if !fence.conn().is_local() && !fence.conn().is_connected() {
        return Err(failed("host_disconnected", format!("{host} 現在連不上，沒有安裝")));
    }
    let gone = |phase: &str| failed("superseded", format!("{host} 在{phase}時重連或改指到另一台，這次安裝作廢"));

    let probe = env.run(&fence, PLATFORM_PROBE_SH, PROBE_TIMEOUT).await.map_err(|e| failed("probe_failed", format!("讀不到 {host} 的作業系統：{e}")))?;
    let platform = platform_of(&probe).ok_or_else(|| failed("unsupported_platform", format!("{host} 的平台 agy 沒有官方安裝包（{}）", probe.trim().replace('\n', " "))))?;
    let manifest = env.manifest(platform).await.map_err(|e| failed("manifest_unavailable", e))?;
    let manifest = parse_manifest(&manifest).map_err(|e| failed("manifest_invalid", e))?;
    if !app.hosts.is_current(&fence).await {
        return Err(gone("讀官方版本"));
    }

    // 已經是官方最新版：不下載、不動檔案（偵測結果可能是舊快取，所以先用腳本問磁碟上現在的版本）。
    let from = cached_version(app, host).await;
    if let Some(v) = from.as_deref().filter(|v| has_version(v, &manifest.version)) {
        return Ok(Installed {
            host: host.into(),
            platform: platform.into(),
            from: Some(v.to_string()),
            to: manifest.version,
            path: crate::tools::cached_path(app, host, "agy").await,
            already_latest: true,
        });
    }

    let script = crate::cli_update::locked_script(INSTALL_LOCK, &install_body(&manifest));
    tracing::info!(host, platform, version = %manifest.version, "installing agy from the official manifest");
    let out = env.run(&fence, &script, INSTALL_TIMEOUT).await.map_err(|e| {
        if e.contains(crate::cli_update::LOCKED_MARK) {
            InstallError::Busy(format!("{host} 已經有另一個 agy 安裝在跑（上一次沒結束、或別的實例開的），這次沒有再裝一次"))
        } else {
            failed("install_failed", format!("在 {host} 安裝 agy 失敗：{e}"))
        }
    })?;
    if !app.hosts.is_current(&fence).await {
        return Err(gone("安裝"));
    }
    let (path, version) = installed_marker(&out).ok_or_else(|| failed("install_failed", format!("安裝腳本跑完了但沒有成功標記：{}", tail(&out))))?;

    // 重新偵測：那台的 PATH 看得到才算裝好（`~/.local/bin` 不在 PATH 的話，bot 與額度探測都找不到 agy）。
    let detected = crate::tools::detect(app, host).await.map_err(|e| failed("verify_failed", format!("agy 已寫入 {path}，但重新偵測 {host} 失敗：{e:#}")))?;
    let seen = detected.tools.get("agy");
    if !seen.is_some_and(|t| t.installed && t.version.as_deref().is_some_and(|v| has_version(v, &manifest.version))) {
        return Err(failed(
            "not_on_path",
            format!("agy {version} 已寫入 {host} 的 {path}，但那台重新偵測沒看到它（多半是 ~/.local/bin 不在 PATH）；請把它加進那台的登入 shell PATH"),
        ));
    }
    Ok(Installed {
        host: host.into(),
        platform: platform.into(),
        from,
        to: manifest.version,
        path: seen.and_then(|t| t.path.clone()).or(Some(path)),
        already_latest: false,
    })
}

#[cfg(test)]
mod tests;
