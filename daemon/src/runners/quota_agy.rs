//! agy quota polling, login and logout runner.

use crate::quota::Quota;
use crate::quota_agy::*;
use crate::state::App;
use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::Duration;

const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(15 * 60);

async fn pane_probe(app: &Arc<App>, host: &str, fence: &crate::hosts::HostFence, exe: &str) -> Result<Vec<(&'static str, Quota)>, ProbeFail> {
    let client = crate::quota_claude::client_for_fence(host, fence).await.map_err(|e| ProbeFail::other("not_connected", format!("{e:#}")))?;
    let home = crate::hosts::home_for_fence(fence).await.map_err(|e| ProbeFail::other("pane", format!("{e:#}")))?;
    if !app.hosts.is_current(fence).await {
        return Err(ProbeFail::other("superseded", format!("host `{host}` changed before its agy quota probe")));
    }
    let label = crate::quota_claude::tagged_probe_label(app, host, PROBE_LABEL_PREFIX.to_string()).await;
    let cmd = pane_probe_command(Some(exe));
    let run = crate::quota_claude::run_marked_pane(
        &client,
        &home,
        &label,
        serde_json::json!({}),
        &cmd,
        PROBE_TIMEOUT,
        PANE_BEGIN,
        is_probe_label,
        pane_done,
        |screen| auth_required(after_begin(screen)),
    )
    .await
    .map_err(|e| ProbeFail::other("pane", format!("could not run the agy quota probe pane on {host}: {e:#}")))?;
    if !app.hosts.is_current(fence).await {
        return Err(ProbeFail::other("superseded", format!("host `{host}` was reconnected/reconfigured during the agy quota probe; stale result discarded")));
    }
    use crate::quota_claude::MarkedRun;
    match run {
        MarkedRun::Done((rc, text)) => read_usage(host, Some(rc), &text),
        MarkedRun::Early(_) => Err(ProbeFail::AuthRequired),
        MarkedRun::TimedOut(screen) => Err(ProbeFail::other(
            "timeout",
            format!("`agy -p /usage` on {host} did not finish within {}s; screen: {}", PROBE_TIMEOUT.as_secs(), tail(after_begin(&screen), 200)),
        )),
    }
}


/// agy 明說沒憑證之後，登入偵測（[`login_watch_once`]）暫時不要又因為 Keychain 裡有項目而把旗標翻回已登入、再開一次 pane。
pub(crate) const AUTH_DENIED_COOLDOWN: Duration = Duration::from_secs(5 * 60);

pub(crate) fn auth_denied() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

pub(crate) fn auth_denied_active(key: &str) -> bool {
    auth_denied().lock().unwrap().get(key).is_some_and(|t| *t > std::time::Instant::now())
}


/// 把這一輪的結果記下來：成功清掉失敗、未登入翻旗標、其他失敗留下原因；有變就推 `host_changed`。
async fn record_probe_result(app: &Arc<App>, host: &str, fence: &crate::hosts::HostFence, result: &Result<(), ProbeFail>) {
    let key = crate::quota::quota_key(host, "agy");
    let mut changed = false;
    match result {
        Ok(()) => {
            auth_denied().lock().unwrap().remove(&key);
            changed |= set_probe_error(host, None);
            // 探測讀得到額度＝這台已登入（登入後第一次探測成功就把「未登入」翻回來）。
            changed |= set_logged_in_quiet(app, host, fence, true).await;
        }
        Err(ProbeFail::AuthRequired) => {
            auth_denied().lock().unwrap().insert(key, std::time::Instant::now() + AUTH_DENIED_COOLDOWN);
            changed |= set_probe_error(host, None);
            changed |= set_logged_in_quiet(app, host, fence, false).await;
        }
        Err(ProbeFail::Other { reason, message }) => {
            changed |= set_probe_error(host, Some(ProbeError { reason, message: message.clone(), at: crate::db::now() }));
        }
    }
    if changed {
        crate::state::emit_host_changed(app, fence).await;
    }
}

/// 一輪探測失敗的原因。`superseded`＝主機在途中換了主機或重連（這個失敗不屬於現在這台，不該記冷卻）。
pub(crate) struct RefreshFail {
    pub error: anyhow::Error,
    pub superseded: bool,
}

impl From<anyhow::Error> for RefreshFail {
    fn from(error: anyhow::Error) -> Self {
        Self { error, superseded: false }
    }
}

/// `Ok(false)` ＝這台主機沒裝 agy（不探測、不報錯）。失敗時錯誤訊息就是原因（同時記在 [`tools_json`] 的 `quota_error`）。
pub async fn refresh_agy(app: &Arc<App>, host: &str) -> Result<bool> {
    refresh_agy_fenced(app, host).await.1.map_err(|f| f.error)
}

/// 同 [`refresh_agy`]，另外回傳這一輪真正用到的 fence（`None` ＝主機不存在），讓呼叫端把冷卻綁在同一個世代上（#873）。
pub(crate) async fn refresh_agy_fenced(app: &Arc<App>, host: &str) -> (Option<crate::hosts::HostFence>, Result<bool, RefreshFail>) {
    let _guard = crate::quota::probe_lock(&format!("{host}#agy")).await;
    let Some(fence) = app.hosts.fence(host).await else { return (None, Err(anyhow!("unknown host `{host}`").into())) };
    let result = refresh_with_fence(app, host, &fence).await;
    (Some(fence), result)
}

async fn refresh_with_fence(app: &Arc<App>, host: &str, fence: &crate::hosts::HostFence) -> Result<bool, RefreshFail> {
    if !app.tools.lock().await.contains_key(host) {
        crate::tools::detect(app, host).await?;
    }
    if !app.hosts.is_current(fence).await {
        return Err(RefreshFail { error: anyhow!("host `{host}` changed before its agy quota probe"), superseded: true });
    }
    let Some(exe) = crate::tools::cached_path(app, host, "agy").await else { return Ok(false) };
    let parsed = if fence.conn().is_local() {
        match crate::hosts::sh_local_capture(&probe_script(Some(&exe)), PROBE_TIMEOUT).await {
            Ok(run) => read_local_run(
                host,
                run.timed_out,
                &String::from_utf8_lossy(&run.stdout),
                &String::from_utf8_lossy(&run.stderr),
                PROBE_TIMEOUT.as_secs(),
            ),
            Err(e) => Err(ProbeFail::other("timeout", format!("{e:#}"))),
        }
    } else {
        pane_probe(app, host, fence, &exe).await
    };
    let buckets = match parsed {
        Ok(b) => b,
        Err(fail) => {
            // 主機換掉了：這個結果不屬於現在這台，別記。
            let superseded = matches!(&fail, ProbeFail::Other { reason: "superseded", .. });
            if !superseded {
                record_probe_result(app, host, fence, &Err(fail.clone())).await;
            }
            let error = match fail {
                ProbeFail::AuthRequired => anyhow!("agy on {host} is not logged in (`agy -p /usage` printed `Authentication required`)"),
                ProbeFail::Other { message, .. } => anyhow!(message),
            };
            return Err(RefreshFail { error, superseded });
        }
    };
    for (key, q) in buckets {
        // 寫入被擋＝途中換代，同樣不屬於現在這台。
        crate::quota::set_fenced(app, host, key, q, fence).await.map_err(|error| RefreshFail { error, superseded: true })?;
    }
    record_probe_result(app, host, fence, &Ok(())).await;
    Ok(true)
}


pub(crate) const REMOTE_LOGOUT_SCRIPT: &str = "f=\"$HOME/.gemini/antigravity-cli/antigravity-oauth-token\"; r=0; \
if [ -e \"$f\" ] || [ -L \"$f\" ]; then rm -f \"$f\" && r=1 || { echo AM_FAILED; exit 0; }; fi; \
if command -v security >/dev/null 2>&1 && security find-generic-password -s gemini -a antigravity >/dev/null 2>&1; then \
security delete-generic-password -s gemini -a antigravity >/dev/null 2>&1 && r=1 || { echo AM_FAILED; exit 0; }; fi; \
[ $r = 1 ] && echo AM_REMOVED || echo AM_ABSENT";

/// 本機 macOS 的 Keychain 項目在不在／刪掉。測試的假 HOME 不會換 Keychain，所以 `cfg(test)` 一律當不存在、不碰——不然測試會讀到（甚至刪掉）真的登入。
#[cfg(all(target_os = "macos", not(test)))]
async fn local_keychain(args: &[&str]) -> bool {
    let run = tokio::process::Command::new("security").args(args).args(["-s", "gemini", "-a", "antigravity"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    matches!(tokio::time::timeout(Duration::from_secs(10), run).await, Ok(Ok(s)) if s.success())
}
#[cfg(not(all(target_os = "macos", not(test))))]
async fn local_keychain(_args: &[&str]) -> bool {
    false
}

/// 憑證在不在：檔案（非空）或 macOS 的 Keychain 項目。遠端跑一小段 sh，本機直接 stat（macOS 再問 `security`）；問不出來是 `None`（不改現有的判斷）。
async fn token_present(fence: &crate::hosts::HostFence) -> Option<bool> {
    if fence.conn().is_local() {
        let home = crate::home::dir()?;
        if std::fs::metadata(home.join(TOKEN_FILE)).map(|m| m.len() > 0).unwrap_or(false) {
            return Some(true);
        }
        return Some(local_keychain(&["find-generic-password"]).await);
    }
    if !fence.conn().is_connected() {
        return None;
    }
    let out = fence.conn().ssh_exec_path_timeout(REMOTE_PRESENT_SCRIPT, Duration::from_secs(10)).await.ok()?;
    match out.trim() {
        "AM_YES" => Some(true),
        "AM_NO" => Some(false),
        _ => None,
    }
}

pub(crate) const REMOTE_PRESENT_SCRIPT: &str = "if [ -s \"$HOME/.gemini/antigravity-cli/antigravity-oauth-token\" ] || { command -v security >/dev/null 2>&1 && security find-generic-password -s gemini -a antigravity >/dev/null 2>&1; }; then echo AM_YES; else echo AM_NO; fi";

/// 把 `tools.agy.logged_in` 寫成 `logged_in`（已知有裝 agy 才寫），有變就推 `host_changed`。額度那格的「未登入」與登入鈕吃這個旗標（網頁 `useLoggedOut`）。
async fn set_logged_in(app: &Arc<App>, host: &str, fence: &crate::hosts::HostFence, logged_in: bool) -> bool {
    let changed = set_logged_in_quiet(app, host, fence, logged_in).await;
    if changed {
        crate::state::emit_host_changed(app, fence).await;
    }
    changed
}

/// 同上但不推：呼叫端還有別的東西要一起變（[`record_probe_result`] 一輪只推一次）。
pub(crate) async fn set_logged_in_quiet(app: &Arc<App>, host: &str, fence: &crate::hosts::HostFence, logged_in: bool) -> bool {
    let mut all = app.tools.lock().await;
    if !app.hosts.is_current(fence).await {
        return false;
    }
    match all.get_mut(host).and_then(|h| h.tools.get_mut("agy")) {
        Some(t) if t.installed && t.logged_in != Some(logged_in) => {
            t.logged_in = Some(logged_in);
            true
        }
        _ => false,
    }
}

async fn agy_state(app: &impl crate::tools::ToolsTable, host: &str) -> Option<(bool, Option<bool>)> {
    app.tools().lock().await.get(host).and_then(|h| h.tools.get("agy")).map(|t| (t.installed, t.logged_in))
}

/// `POST /api/hosts/{name}/agy/logout`：刪掉那台主機的 agy 憑證檔，再清掉那台 agy 的額度快照（`agy`）並廣播，
/// 那一格立刻變成沒有讀數。回「檔案原本在不在」（不在不算錯）。不跑 agy、不動別的檔、不停正在跑的 agy bot（它們下次重啟才會停在登入畫面）。
/// 拿探測的鎖：正在跑的探測不能在清掉之後又把讀數寫回來。
pub async fn logout(app: &Arc<App>, host: &str) -> Result<bool, LogoutError> {
    // 請求當下的權威：排隊等探測鎖期間 H 若換了主機或重連，這份就作廢（#865）。
    let requested = app.hosts.fence(host).await.ok_or(LogoutError::UnknownHost)?;
    let _guard = crate::quota::probe_lock(&format!("{host}#agy")).await;
    let fence = app.hosts.fence(host).await.ok_or(LogoutError::UnknownHost)?;
    if !fence.same_authority(&requested) {
        return Err(LogoutError::Superseded);
    }
    // 刪憑證 → 清額度 → 翻未登入，整段拿 authority_gate 的 read 端，跟 repoint／reconnect 線性化。
    // closure 裡不能呼叫 `emit_host_changed`／`set_logged_in`（它們內部也走 `run_if_current`，寫者排隊時再取 read 會死結），推播放到外面。
    let done = app
        .hosts
        .run_if_current(&fence, async {
            let removed = if fence.conn().is_local() {
                let home = crate::home::dir().ok_or_else(|| LogoutError::Failed("no home directory".into()))?;
                let file = match std::fs::remove_file(home.join(TOKEN_FILE)) {
                    Ok(()) => true,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                    Err(e) => return Err(LogoutError::Failed(format!("cannot remove the agy credentials: {e}"))),
                };
                let keychain = local_keychain(&["find-generic-password"]).await;
                if keychain && !local_keychain(&["delete-generic-password"]).await {
                    return Err(LogoutError::Failed("cannot remove the agy credentials from the Keychain".into()));
                }
                file || keychain
            } else {
                if !fence.conn().is_connected() {
                    return Err(LogoutError::Failed(format!("host `{host}` is not connected")));
                }
                let out = fence.conn().ssh_exec_path_timeout(REMOTE_LOGOUT_SCRIPT, PROBE_TIMEOUT).await.map_err(|e| LogoutError::Failed(format!("{e:#}")))?;
                match out.trim() {
                    "AM_REMOVED" => true,
                    "AM_ABSENT" => false,
                    other => return Err(LogoutError::Failed(format!("remote agy logout did not confirm: {other}"))),
                }
            };
            let key = crate::quota::quota_key(host, "agy");
            let had = app.quotas.lock().await.contains_key(&key);
            crate::quota::forget(app, &key).await;
            // 額度那格要立刻變成「未登入」並出現登入鈕，不等下一輪偵測。
            let changed = set_logged_in_quiet(app, host, &fence, false).await;
            Ok::<_, LogoutError>((removed, had, changed))
        })
        .await
        .ok_or(LogoutError::Superseded)?;
    let (removed, had, changed) = done?;
    if had {
        let key = crate::quota::quota_key(host, "agy");
        app.emit("quota_updated", serde_json::json!({"kind": key, "host": host, "quota": null})).await;
    }
    if changed {
        crate::state::emit_host_changed(app, &fence).await;
    }
    tracing::info!(host, removed, "agy logged out: credentials removed and quota snapshots cleared");
    Ok(removed)
}


/// 探測失敗的冷卻。綁著失敗當下的主機世代（同 `update_watch::DiskVersionEntry`）：
/// 同名改指到別台或重連之後，舊世代的冷卻不能壓住新主機，舊世代的 watcher 也不能清掉新主機的冷卻（#873）。
pub(crate) struct BackoffEntry {
    pub until: std::time::Instant,
    pub authority: crate::hosts::HostAuthorityKey,
}

pub(crate) fn backoff() -> &'static std::sync::Mutex<std::collections::HashMap<String, BackoffEntry>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, BackoffEntry>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// 這個世代還在冷卻嗎？世代對不上的紀錄屬於舊主機，順手丟掉。
pub(crate) fn backoff_active(key: &str, fence: &crate::hosts::HostFence) -> bool {
    let mut map = backoff().lock().unwrap();
    match map.get(key) {
        Some(entry) if entry.authority.matches(fence) => entry.until > std::time::Instant::now(),
        Some(_) => {
            map.remove(key);
            false
        }
        None => false,
    }
}

/// 只有「主機還是這個世代」才記冷卻；換代的失敗（`superseded`）不記。回 true ＝寫進去了。
pub(crate) async fn note_failure(app: &Arc<App>, host: &str, fence: &crate::hosts::HostFence) -> bool {
    let key = crate::quota::quota_key(host, "agy");
    app.hosts
        .run_if_current(fence, async {
            backoff().lock().unwrap().insert(key, BackoffEntry { until: std::time::Instant::now() + RETRY_AFTER_FAILURE, authority: fence.authority_key() });
        })
        .await
        .is_some()
}

/// 登入偵測發現已登入時清冷卻：只清自己這個世代寫的那筆。
pub(crate) async fn clear_backoff(app: &Arc<App>, host: &str, fence: &crate::hosts::HostFence) {
    let key = crate::quota::quota_key(host, "agy");
    app.hosts
        .run_if_current(fence, async {
            let mut map = backoff().lock().unwrap();
            if map.get(&key).is_some_and(|entry| entry.authority.matches(fence)) {
                map.remove(&key);
            }
        })
        .await;
}

/// 這個失敗要不要開始冷卻：換代造成的失敗不算。
pub(crate) fn starts_backoff(fail: &RefreshFail) -> bool {
    !fail.superseded
}

/// `None` ＝這一輪跳過（上次失敗還在冷卻）。`GET /api/quota?refresh=1` 不走這裡，一律真的探測。
pub async fn refresh_agy_if_due(app: &Arc<App>, host: &str) -> Result<Option<bool>> {
    // 已知未登入：`agy -p /usage` 會停在登入畫面等到逾時，不探測；登入後由 [`spawn_agy_login_watcher`] 發現憑證檔再探。
    if matches!(agy_state(app, host).await, Some((true, Some(false)))) {
        return Ok(None);
    }
    refresh_agy_gated(app, host).await
}

/// 同上但**不看**已知未登入的旗標：登入偵測發現憑證檔出現時，就是要靠這一次探測決定旗標該不該翻（issue #870）。
/// 探測成功 [`record_probe_result`] 會把旗標翻成已登入並推 `host_changed`；明確 auth 失敗維持未登入並記 5 分鐘冷卻；
/// 其他失敗（網路、逾時、pane）旗標不動、只記探測錯誤，並進 15 分鐘失敗冷卻（不然每 20 秒就起一次 200 MB 的探測）。
async fn refresh_agy_gated(app: &Arc<App>, host: &str) -> Result<Option<bool>> {
    let key = crate::quota::quota_key(host, "agy");
    let Some(cur) = app.hosts.fence(host).await else { return Ok(None) };
    if backoff_active(&key, &cur) {
        return Ok(None);
    }
    let (fence, result) = refresh_agy_fenced(app, host).await;
    match result {
        Ok(v) => {
            // 探測通了：這個世代留著的過期冷卻紀錄一併清掉。
            if let Some(fence) = fence.as_ref() {
                clear_backoff(app, host, fence).await;
            }
            Ok(Some(v))
        }
        Err(fail) => {
            if let Some(fence) = fence.as_ref().filter(|_| starts_backoff(&fail)) {
                note_failure(app, host, fence).await;
            }
            Err(fail.error)
        }
    }
}

pub fn spawn_agy_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            crate::quota::for_each_host(crate::quota::pollable_hosts(&app).await, |host| {
                let app = app.clone();
                async move {
                    match refresh_agy_if_due(&app, &host).await {
                        Ok(Some(true) | None) => {}
                        Ok(Some(false)) => tracing::debug!(host = %host, "agy not installed; agy quota stays null"),
                        Err(e) => tracing::warn!(host = %host, error = %e, retry_in_s = RETRY_AFTER_FAILURE.as_secs(), "agy quota refresh failed; keeping the last reading"),
                    }
                }
            })
            .await;
            tokio::time::sleep(AGY_POLL).await;
        }
    });
}

/// 登入偵測的間隔：只對「裝了 agy、目前記成未登入」的主機做一次 stat（遠端是一個很小的 ssh），或補一次還沒有讀數的探測。
pub const LOGIN_WATCH: Duration = Duration::from_secs(20);

/// 使用者在 shell 裡把 agy 登好之後，不重啟、不等 5 分鐘輪詢：憑證檔一出現就**先探測** `/usage`，探測成功才把
/// `tools.agy.logged_in` 翻成已登入（兩條額度同時回到那一格）。憑證檔只證明有一份 credential，不證明 agy 還接受它
/// （過期、被撤銷時檔案還在），所以不能先翻再說（issue #870）：
/// - 明確 auth 失敗（`Authentication required`，含逾時前已印出的）：維持未登入，5 分鐘內不再因憑證檔還在而探測；
/// - 其他失敗（網路、逾時、pane）：旗標保持現值、記探測錯誤，15 分鐘後才再試；
/// - 探測成功：[`record_probe_result`] 翻旗標、清冷卻、推 `host_changed`。
/// 另外，已登入但還沒有任何讀數（例如手動「重新偵測」才翻成已登入）也補探一次。
pub async fn login_watch_once(app: &Arc<App>, host: &str) {
    // 沒裝、或這輪偵測沒問出登入與否（`None`）：什麼都不猜。
    let Some((true, Some(logged_in))) = agy_state(app, host).await else { return };
    let Some(fence) = app.hosts.fence(host).await else { return };
    let key = crate::quota::quota_key(host, "agy");
    let probe = if !logged_in {
        // agy 剛明說沒憑證：Keychain 裡有項目（可能是過期的 token）不算登好了，冷卻期內不探測、不開 pane。
        if auth_denied_active(&key) || token_present(&fence).await != Some(true) {
            return;
        }
        refresh_agy_gated(app, host).await
    } else if app.quotas.lock().await.contains_key(&key) {
        return;
    } else {
        refresh_agy_if_due(app, host).await
    };
    if let Err(e) = probe {
        tracing::warn!(host, error = %e, "agy quota probe after login failed");
    }
}

pub fn spawn_agy_login_watcher(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LOGIN_WATCH).await;
            for host in crate::quota::pollable_hosts(&app).await {
                login_watch_once(&app, &host).await;
            }
        }
    });
}
