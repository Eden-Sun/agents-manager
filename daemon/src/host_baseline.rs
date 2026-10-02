//! 主機 CLI 工作環境一致性檢查（issue #719，SPEC §16.7）：**只讀、只報告，不修、不裝、不寫任何檔**。
//!
//! 跟 [`crate::tools::PROBE_SH`] 同一趟探測（同一次 ssh）：[`BASELINE_SH`] 接在它後面，印 `AM_BL …` 行；
//! [`evaluate`] 把這些行對照 [`REQUIRED_TOOLS`] 等基準，產生 `hosts[].baseline`。
//! 探測沒跑完（沒有結尾標記）就是 `None`（未知），不是「全部缺」——否則一次 ssh 逾時就會在每台主機上喊一排缺漏。

use serde::Serialize;

/// 缺了就不能好好用的：嚴重項。其餘是提醒。
pub const CRITICAL: &str = "critical";
pub const WARN: &str = "warn";

/// 每台主機都該有的工具（claude／codex／grok 是各主機自己決定要不要裝，由 `tools` 回報，不在這裡）。
pub const REQUIRED_TOOLS: [&str; 6] = ["herdr", "rtk", "zsh", "bun", "jq", "gh"];

/// 每個 claude 身分的設定目錄裡該有的檔案：`(檔名, 嚴重度)`。
const CLAUDE_FILES: [(&str, &str); 4] =
    [("settings.json", CRITICAL), ("statusline-command.sh", CRITICAL), ("CLAUDE.md", WARN), ("RTK.md", WARN)];
/// `settings.json` 該有的頂層鍵（只看有沒有，不比內容：內容是各主機自己的）。
const CLAUDE_KEYS: [(&str, &str); 3] = [("statusLine", CRITICAL), ("hooks", CRITICAL), ("permissions", WARN)];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BaselineIssue {
    /// 穩定的識別字，例如 `tool.rtk`、`claude.cc1.settings.json:statusLine`。
    pub id: String,
    /// `critical` | `warn`
    pub severity: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BaselineReport {
    /// 那台主機的 `uname -s`（`Darwin`／`Linux`）；探測沒帶＝`None`。Mac 專用項只在 Darwin 上才算數。
    pub os: Option<String>,
    /// 探測沒跑完＝`None`（未知）；跑完而且一致＝`Some([])`。
    pub issues: Option<Vec<BaselineIssue>>,
    pub checked_at: String,
}

/// Mac 專用的 claude plugin（使用者 2026-09-28：imessage／discord）：只在 Darwin 主機上才算「該有」，Linux 不列為缺。
/// 腳本裡的 `MAC_ONLY_PLUGINS` 與這裡同步（`mac_only_plugins_match_the_script` 測試守著）。
pub const MAC_ONLY_PLUGINS: [&str; 2] = ["imessage", "discord"];

/// 只讀。接在 `PROBE_SH` 後面（用它定義的 `am_abs`）。身分＝`$CLAUDE_CONFIG_DIR`、`~/.claude`、`~/.claude-cc<N>`。
pub const BASELINE_SH: &str = r#"
printf 'AM_BL begin\n'
MAC_ONLY_PLUGINS="imessage discord"
printf 'AM_BL os %s\n' "$(uname -s 2>/dev/null)"
# 六個工具一次問完：每次 `$SHELL -lic` 都要讀完整個 rc（nvm／conda 動輒數秒），PROBE_SH 自己已經開了好幾次，
# 再各開六次會把整趟探測推過 ssh 的 30 秒上限，連原本的 tools 偵測都跟著失敗。只認絕對路徑（alias 的字串不算，#666）。
bl_paths=$( "${SHELL:-/bin/sh}" -lic 'for t in herdr rtk zsh bun jq gh; do printf "AM_BLP %s %s\n" "$t" "$(command -v "$t" 2>/dev/null | tail -1)"; done' 2>/dev/null </dev/null | grep '^AM_BLP ' )
for t in herdr rtk zsh bun jq gh; do
  p=$(printf '%s\n' "$bl_paths" | sed -n "s/^AM_BLP $t //p" | tail -1)
  case "$p" in /*) ;; *) p="" ;; esac
  [ -n "$p" ] || p=$(command -v "$t" 2>/dev/null)
  case "$p" in /*) ;; *) p="" ;; esac
  printf 'AM_BL tool %s %s\n' "$t" "$p"
done
bl_has() { [ -e "$1" ] && printf 1 || printf 0; }
bl_plugin() { [ -f "$1" ] && grep -Eq "\"$2@[^\"]*\"[[:space:]]*:[[:space:]]*true" "$1" 2>/dev/null && printf 1 || printf 0; }
bl_key() { [ -f "$1" ] && grep -Eq "\"$2\"[[:space:]]*:" "$1" 2>/dev/null && printf 1 || printf 0; }
for d in "${CLAUDE_CONFIG_DIR:-}" "$HOME/.claude" "$HOME"/.claude-cc[0-9]*; do
  [ -n "$d" ] && [ -d "$d" ] || continue
  case "$d" in "$HOME/.claude") n=default ;; "$HOME"/.claude-cc*) n=${d##*/.claude-} ;; *) n=custom ;; esac
  # `.claude-cc<數字>*` 的 glob 什麼名字都收：身分名只認一般字元，空白、換行這類會讓欄位錯位或偽造 `AM_BL` 行的整個跳過。
  # 路徑本身不輸出（evaluate 不用它）。
  case "$n" in *[!A-Za-z0-9_.-]*) continue ;; esac
  printf 'AM_BL claude-dir %s\n' "$n"
  for f in settings.json statusline-command.sh CLAUDE.md RTK.md; do
    printf 'AM_BL claude-file %s %s %s\n' "$n" "$f" "$(bl_has "$d/$f")"
  done
  for k in statusLine hooks permissions; do
    printf 'AM_BL claude-key %s %s %s\n' "$n" "$k" "$(bl_key "$d/settings.json" "$k")"
  done
  for pl in $MAC_ONLY_PLUGINS; do
    printf 'AM_BL claude-plugin %s %s %s\n' "$n" "$pl" "$(bl_plugin "$d/settings.json" "$pl")"
  done
done
CX="${CODEX_HOME:-$HOME/.codex}"
printf 'AM_BL codex-file config.toml %s\n' "$(bl_has "$CX/config.toml")"
printf 'AM_BL codex-file hooks.json %s\n' "$(bl_has "$CX/hooks.json")"
if [ -f "$CX/config.toml" ] && grep -Eq '^[[:space:]]*approval_policy[[:space:]]*=' "$CX/config.toml" 2>/dev/null; then
  printf 'AM_BL codex-key approval_policy 1\n'
else
  printf 'AM_BL codex-key approval_policy 0\n'
fi
printf 'AM_BL grok-file config.toml %s\n' "$(bl_has "${GROK_HOME:-$HOME/.grok}/config.toml")"
printf 'AM_BL herdr-file config.toml %s\n' "$(bl_has "${XDG_CONFIG_HOME:-$HOME/.config}/herdr/config.toml")"
if [ -f "$HOME/.gitconfig" ] && grep -Eq 'https?://([^/@[:space:]]+:[^/@[:space:]]+|(gh[pousr]_|github_pat_|glpat-)[A-Za-z0-9_-]+)@' "$HOME/.gitconfig" 2>/dev/null; then
  printf 'AM_BL gitconfig-token 1\n'
else
  printf 'AM_BL gitconfig-token 0\n'
fi
printf 'AM_BL end\n'
"#;

/// 探測輸出裡的 `AM_BL os <uname -s>`。
pub fn os_of(out: &str) -> Option<String> {
    out.lines().find_map(|l| l.trim_end().strip_prefix("AM_BL os ")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 差異有東西才推 inbox：回 `(event_key, payload)`。同一份差異（id 集合相同，與順序無關）key 相同，
/// `push_inbox` 的 `INSERT OR IGNORE` 就只留第一則；差異變了 key 才不同。未知（`issues: None`）或一致都不推。
pub fn alert_for(host: &str, report: &BaselineReport) -> Option<(String, serde_json::Value)> {
    let issues = report.issues.as_ref().filter(|i| !i.is_empty())?;
    let mut ids: Vec<&str> = issues.iter().map(|i| i.id.as_str()).collect();
    ids.sort_unstable();
    let key = format!("ops_alert:daemon:host_baseline:{host}:{}", crate::supervisor::cli_refresh::short_hash(ids.join("\n").as_bytes()));
    let critical = issues.iter().filter(|i| i.severity == CRITICAL).count();
    let payload = serde_json::json!({
        "source": "daemon",
        "reason": "host_baseline",
        "subject": host,
        "detail": format!("主機 `{host}` 的 CLI 工作環境跟基準不一致：{} 項（{critical} 項嚴重）", issues.len()),
        "critical": critical,
        "issues": issues,
        "checked_at": report.checked_at,
        "action": "只是報告，daemon 什麼都沒改：看 issues 決定要不要補（缺的工具、statusline、hook、設定檔）；補完下一次偵測（重連、每 6 小時、`POST /api/hosts/{name}/tools/refresh`）會重量。同一份差異只推這一次，差異變了才會再推。",
    });
    Some((key, payload))
}

/// 把 [`alert_for`] 推進 AGM inbox（`ops_alert`，`source=daemon`）。推不進去只記 log：檢查本身不能因此失敗。
pub async fn notify(app: &std::sync::Arc<crate::state::App>, host: &str, report: &BaselineReport) {
    let Some((key, payload)) = alert_for(host, report) else { return };
    match crate::supervisor::store::push_inbox(&app.db, &key, "ops_alert", None, None, None, &payload).await {
        Ok(Some(_)) => tracing::warn!(host, issues = report.issues.as_ref().map_or(0, Vec::len), "host baseline differs; ops_alert queued"),
        Ok(None) => {}
        Err(e) => tracing::error!(host, error = %e, "host baseline differs; ops_alert could not be queued"),
    }
}

/// 定期重量的間隔：偵測本來只在連上、alias 變了、手動時才跑，設定被改掉（或補好）要等到下一次才看得到。
pub const RECHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// 每隔 [`RECHECK_EVERY`] 對每台連著的主機重跑一次偵測（含這份檢查）；啟動那一輪由開機偵測負責，所以先睡再做。
pub fn spawn_poller(app: std::sync::Arc<crate::state::App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(RECHECK_EVERY).await;
            for name in app.hosts.names().await {
                let connected = match app.hosts.get(&name).await {
                    Some(c) if c.is_local() => app.connected.load(std::sync::atomic::Ordering::SeqCst),
                    Some(c) => c.is_connected(),
                    None => false,
                };
                if connected {
                    crate::tools::spawn_detect(app.clone(), name);
                }
            }
        }
    });
}

fn issue(id: String, severity: &'static str, message: String) -> BaselineIssue {
    BaselineIssue { id, severity, message }
}

fn severity_of(table: &[(&str, &'static str)], name: &str) -> &'static str {
    table.iter().find(|(n, _)| *n == name).map_or(WARN, |(_, s)| s)
}

/// 解 [`BASELINE_SH`] 的輸出，照探測印出的順序列出缺漏。沒有 `AM_BL end`（探測被截斷、沒跑到）＝`None`。
///
/// 檔案本身不在時不再列裡面的鍵（`settings.json` 沒有就不用再說 `statusLine`、`hooks` 沒有）：一個根因一行。
pub fn evaluate(out: &str) -> Option<Vec<BaselineIssue>> {
    if !out.lines().any(|l| l.trim_end() == "AM_BL end") {
        return None;
    }
    let mac = os_of(out).as_deref() == Some("Darwin");
    let mut issues = Vec::new();
    // 目前這個 claude 身分的 settings.json 在不在、codex 的 config.toml 在不在：決定要不要看裡面的鍵。
    let mut claude_settings = true;
    let mut codex_config = true;
    for line in out.lines() {
        let f: Vec<&str> = line.trim_end().splitn(5, ' ').collect();
        if f.first() != Some(&"AM_BL") || f.len() < 2 {
            continue;
        }
        let present = |v: Option<&&str>| v.copied() == Some("1");
        match f[1] {
            "tool" if f.len() >= 3 => {
                if f.get(3).map_or(true, |p| p.is_empty()) && REQUIRED_TOOLS.contains(&f[2]) {
                    issues.push(issue(format!("tool.{}", f[2]), CRITICAL, format!("缺工具 {}（登入 shell 的 PATH 找不到）", f[2])));
                }
            }
            "claude-file" if f.len() >= 5 => {
                let present = present(f.get(4));
                if f[3] == "settings.json" {
                    claude_settings = present;
                }
                if !present {
                    issues.push(issue(
                        format!("claude.{}.{}", f[2], f[3]),
                        severity_of(&CLAUDE_FILES, f[3]),
                        format!("claude 身分 {}：缺 {}", f[2], f[3]),
                    ));
                }
            }
            "claude-key" if f.len() >= 5 => {
                if claude_settings && !present(f.get(4)) {
                    issues.push(issue(
                        format!("claude.{}.settings.json:{}", f[2], f[3]),
                        severity_of(&CLAUDE_KEYS, f[3]),
                        format!("claude 身分 {}：settings.json 沒有 {}", f[2], f[3]),
                    ));
                }
            }
            "claude-plugin" if f.len() >= 5 => {
                // Mac 專用：Linux（或不知道是什麼系統）一律不算缺；settings.json 本身不在也不再追問。
                if mac && claude_settings && MAC_ONLY_PLUGINS.contains(&f[3]) && !present(f.get(4)) {
                    issues.push(issue(
                        format!("claude.{}.plugin:{}", f[2], f[3]),
                        WARN,
                        format!("claude 身分 {}：沒有啟用 {} plugin（Mac 專用）", f[2], f[3]),
                    ));
                }
            }
            "codex-file" if f.len() >= 4 => {
                let present = present(f.get(3));
                if f[2] == "config.toml" {
                    codex_config = present;
                }
                if !present {
                    issues.push(issue(format!("codex.{}", f[2]), WARN, format!("codex：缺 {}", f[2])));
                }
            }
            "codex-key" if f.len() >= 4 => {
                if codex_config && !present(f.get(3)) {
                    issues.push(issue(format!("codex.config.toml:{}", f[2]), WARN, format!("codex：config.toml 沒有 {}", f[2])));
                }
            }
            "grok-file" | "herdr-file" if f.len() >= 4 => {
                if !present(f.get(3)) {
                    let who = f[1].trim_end_matches("-file");
                    issues.push(issue(format!("{who}.{}", f[2]), WARN, format!("{who}：缺 {}", f[2])));
                }
            }
            "gitconfig-token" if f.len() >= 3 => {
                if present(f.get(2)) {
                    issues.push(issue(
                        "gitconfig.token".into(),
                        WARN,
                        "~/.gitconfig 內嵌帶密碼的網址（只報有，不顯示內容）".into(),
                    ));
                }
            }
            _ => {}
        }
    }
    // CLAUDE_CONFIG_DIR 指到 ~/.claude 或 ~/.claude-ccN 時，同一個目錄會走到兩次：同一個 id 只報一次。
    let mut seen = std::collections::HashSet::new();
    issues.retain(|i| seen.insert(i.id.clone()));
    Some(issues)
}

#[cfg(test)]
mod tests;
