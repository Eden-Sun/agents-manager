//! 主機 CLI 工作環境一致性檢查（issue #719，SPEC §16.7）：**只讀、只報告，不修、不裝、不寫任何檔**。
//!
//! 跟 [`crate::tools::PROBE_SH`] 同一趟探測（同一次 ssh）：[`BASELINE_SH`] 接在它後面，印 `AM_BL …` 行；
//! [`evaluate`] 把這些行對照 [`REQUIRED_TOOLS`] 等基準，產生 `hosts[].baseline`。
//! 探測沒跑完（沒有結尾標記）就是 `None`（未知），不是「全部缺」——否則一次 ssh 逾時就會在每台主機上喊一排缺漏。

use serde::Serialize;


/// 基準檢查（偵測結果的一部分）需要從 `App` 拿的外部事實（`App` 在 `app_ports_p3` 實作）：主機表、最近一次結果快取、本機 herdr 連線狀態、
/// 把差異推進 AGM inbox。
pub trait BaselineEnv: crate::hosts::HostsAccess + 'static {
    /// 每台主機最近一次的基準結果（`app.host_baseline`）。
    fn baseline_cache(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, BaselineReport>>;
    /// 本機 herdr 現在連著嗎（本機沒有 ssh master，看這個）。
    fn local_herdr_connected(&self) -> bool;
    /// 推一則 `ops_alert`（`source=daemon`）進 AGM inbox：`Ok(true)`＝新推進去，`Ok(false)`＝同 key 已有。
    fn push_ops_alert(&self, key: &str, payload: &serde_json::Value) -> impl std::future::Future<Output = anyhow::Result<bool>> + Send;
}

/// 缺了就不能好好用的：嚴重項。其餘是提醒。
pub const CRITICAL: &str = "critical";
pub const WARN: &str = "warn";

/// 每台主機都該有的工具（claude／codex／grok 是各主機自己決定要不要裝，由 `tools` 回報，不在這裡）。
pub const REQUIRED_TOOLS: [&str; 6] = ["herdr", "rtk", "zsh", "bun", "jq", "gh"];

/// 每個 claude 身分的設定目錄裡該有的檔案：`(檔名, 嚴重度)`。
const CLAUDE_FILES: [(&str, &str); 4] =
    [("settings.json", CRITICAL), ("statusline-command.sh", CRITICAL), ("CLAUDE.md", WARN), ("RTK.md", WARN)];
/// `settings.json` 該有的鍵（只看有沒有，不比內容：內容是各主機自己的）。
const CLAUDE_KEYS: [(&str, &str); 4] =
    [("statusLine", CRITICAL), ("hooks", CRITICAL), ("permissions", WARN), ("defaultMode", WARN)];

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
    /// 這份結果是**什麼時候量到的**（最後一次成功的偵測）；偵測失敗不改寫它。
    pub checked_at: String,
    /// 最後一次偵測失敗的時間；之後成功就清掉。有值＝上面的結果是舊的、現在連不上／探測失敗。
    pub failed_at: Option<String>,
    /// 失敗原因，只留第一行（ssh 的錯誤很長，也可能帶主機細節）。
    pub error: Option<String>,
    /// 讀取時才算（[`BaselineReport::snapshot`]）：最後一次偵測失敗，或超過一個重量週期沒更新。存在記憶體裡的一律是 `false`。
    pub stale: bool,
}

/// 超過 [`RECHECK_EVERY`] 再寬限這麼久才算「太久沒量」：定期重量剛好在週期上，偵測本身又要花幾十秒，不寬限的話
/// 每個週期交界都會閃一下過期。
pub const STALE_GRACE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

impl BaselineReport {
    /// 對外給的樣子：把 `stale` 算進去。偵測失敗，或 `checked_at` 比 `RECHECK_EVERY + STALE_GRACE` 還舊（讀不懂也當舊）＝過期。
    /// 舊結果照樣留著給人看，只是標明不是現在的。
    pub fn snapshot(&self, now: chrono::DateTime<chrono::Utc>) -> BaselineReport {
        let limit = chrono::Duration::from_std(RECHECK_EVERY + STALE_GRACE).unwrap_or_else(|_| chrono::Duration::hours(7));
        let old = chrono::DateTime::parse_from_rfc3339(&self.checked_at)
            .map(|t| now.signed_duration_since(t.with_timezone(&chrono::Utc)) > limit)
            .unwrap_or(true);
        BaselineReport { stale: self.failed_at.is_some() || old, ..self.clone() }
    }
}

/// 偵測失敗（ssh 逾時、連不上、探測腳本沒跑起來）：已經有舊結果就標上失敗時間與原因，結果本身與 `checked_at` 原封不動；
/// 沒量過就什麼都不造（維持「尚未檢查」）。
pub async fn note_failure(app: &impl BaselineEnv, host: &str, err: &anyhow::Error) {
    let mut map = app.baseline_cache().lock().await;
    let Some(report) = map.get_mut(host) else { return };
    let first_line = format!("{err:#}").lines().next().unwrap_or_default().chars().take(200).collect::<String>();
    report.failed_at = Some(crate::db::now());
    report.error = Some(first_line);
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
# HISTFILE=/dev/null：macOS /bin/sh 是 bash 3.2，-i 會把 history 寫進 $HOME。這支腳本必須只讀。
bl_paths=$( HISTFILE=/dev/null "${SHELL:-/bin/sh}" -lic 'for t in herdr rtk zsh bun jq gh; do printf "AM_BLP %s %s\n" "$t" "$(command -v "$t" 2>/dev/null | tail -1)"; done' 2>/dev/null </dev/null | grep '^AM_BLP ' )
bl_jq_path=$(printf '%s\n' "$bl_paths" | sed -n 's/^AM_BLP jq //p' | tail -1)
case "$bl_jq_path" in /*) [ -x "$bl_jq_path" ] || bl_jq_path="" ;; *) bl_jq_path=$(command -v jq 2>/dev/null || true) ;; esac
for t in herdr rtk zsh bun jq gh; do
  p=$(printf '%s\n' "$bl_paths" | sed -n "s/^AM_BLP $t //p" | tail -1)
  case "$p" in /*) ;; *) p="" ;; esac
  [ -n "$p" ] || p=$(command -v "$t" 2>/dev/null)
  case "$p" in /*) ;; *) p="" ;; esac
  printf 'AM_BL tool %s %s\n' "$t" "$p"
done
bl_has() { [ -e "$1" ] && printf 1 || printf 0; }
bl_plugin() { [ -f "$1" ] && grep -Eq "\"$2@[^\"]*\"[[:space:]]*:[[:space:]]*true" "$1" 2>/dev/null && printf 1 || printf 0; }
bl_key() {
  [ -f "$1" ] || { printf 0; return; }
  [ -n "$bl_jq_path" ] && [ -x "$bl_jq_path" ] || { printf 0; return; }
  if [ "$2" = defaultMode ]; then
    "$bl_jq_path" -e 'type == "object" and (.permissions | type == "object" and has("defaultMode"))' "$1" >/dev/null 2>&1 && printf 1 || printf 0
  else
    "$bl_jq_path" -e --arg key "$2" 'type == "object" and has($key)' "$1" >/dev/null 2>&1 && printf 1 || printf 0
  fi
}
# `features.hooks = true`，或 `[features]` 表裡的 `hooks = true`。false／沒寫＝0。不開 login shell。
bl_features_hooks() {
  [ -f "$1" ] || { printf 0; return; }
  /usr/bin/awk '
    BEGIN { root = 1 }
    {
      line = $0
      sub(/\r$/, "", line)
      sub(/^[[:blank:]]*/, "", line)
      if (line == "" || line ~ /^#/) next
      if (line ~ /^\[/) {
        in_features = (line ~ /^\[features\][[:blank:]]*(#.*)?$/)
        root = 0
        next
      }
      sub(/[[:blank:]]+#.*/, "", line)
      if ((root && line ~ /^features\.hooks[[:blank:]]*=[[:blank:]]*true[[:blank:]]*$/) ||
          (in_features && line ~ /^hooks[[:blank:]]*=[[:blank:]]*true[[:blank:]]*$/)) found = 1
    }
    END { exit !found }
  ' "$1" && printf 1 || printf 0
}
# 同一個目錄只查一次。身分名只認一般字元；路徑不印出來。
bl_seen="|"
bl_claude() {
  d=$1
  n=$2
  case "$n" in *[!A-Za-z0-9_.-]*) return ;; esac
  [ -n "$d" ] && [ -d "$d" ] || return
  case "$bl_seen" in *"|$d|"*) return ;; esac
  bl_seen="${bl_seen}${d}|"
  printf 'AM_BL claude-dir %s\n' "$n"
  for f in settings.json statusline-command.sh CLAUDE.md RTK.md; do
    printf 'AM_BL claude-file %s %s %s\n' "$n" "$f" "$(bl_has "$d/$f")"
  done
  for k in statusLine hooks permissions defaultMode; do
    printf 'AM_BL claude-key %s %s %s\n' "$n" "$k" "$(bl_key "$d/settings.json" "$k")"
  done
  for pl in $MAC_ONLY_PLUGINS; do
    printf 'AM_BL claude-plugin %s %s %s\n' "$n" "$pl" "$(bl_plugin "$d/settings.json" "$pl")"
  done
}
for d in "${CLAUDE_CONFIG_DIR:-}" "$HOME/.claude" "$HOME"/.claude-cc[0-9]*; do
  [ -n "$d" ] && [ -d "$d" ] || continue
  case "$d" in "$HOME/.claude") n=default ;; "$HOME"/.claude-cc*) n=${d##*/.claude-} ;; *) n=custom ;; esac
  bl_claude "$d" "$n"
done
# 登入 shell 的 alias（PROBE_SH 留在 $al；單獨跑這段時改讀 ~/.zshrc，不再開一次 login shell）。
# cc0–cc6 若 CLAUDE_CONFIG_DIR 不在上面的 glob（例如 ~/.claude-work）也要查。
[ -n "${al:-}" ] || al=$(cat "$HOME/.zshrc" 2>/dev/null || true)
printf '%s\n' "$al" | grep -E '(^|[[:space:]])(alias[[:space:]]+)?cc[0-6]=' | while IFS= read -r line; do
  name=$(printf '%s\n' "$line" | sed -n 's/.*\(cc[0-6]\)=.*/\1/p' | head -1)
  dir=$(printf '%s\n' "$line" | sed -n 's/.*CLAUDE_CONFIG_DIR=["'\'']\{0,1\}\([^[:space:]"'\'']*\).*/\1/p' | head -1)
  case "$dir" in
    '${HOME}/'*) dir="$HOME/${dir#'${HOME}/'}" ;;
    \$HOME/*) dir="$HOME/${dir#\$HOME/}" ;;
    "~/"*) dir="$HOME/${dir#~/}" ;;
  esac
  case "$dir" in /*) ;; *) continue ;; esac
  [ -n "$name" ] || continue
  bl_claude "$dir" "$name"
done
CX="${CODEX_HOME:-$HOME/.codex}"
printf 'AM_BL codex-file config.toml %s\n' "$(bl_has "$CX/config.toml")"
printf 'AM_BL codex-file hooks.json %s\n' "$(bl_has "$CX/hooks.json")"
if [ -f "$CX/config.toml" ] && grep -Eq '^[[:space:]]*approval_policy[[:space:]]*=' "$CX/config.toml" 2>/dev/null; then
  printf 'AM_BL codex-key approval_policy 1\n'
else
  printf 'AM_BL codex-key approval_policy 0\n'
fi
printf 'AM_BL codex-key features.hooks %s\n' "$(bl_features_hooks "$CX/config.toml")"
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
    let key = format!("ops_alert:daemon:host_baseline:{host}:{}", crate::supervisor_inbox::short_hash(ids.join("\n").as_bytes()));
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
pub async fn notify(app: &impl BaselineEnv, host: &str, report: &BaselineReport) {
    let Some((key, payload)) = alert_for(host, report) else { return };
    match app.push_ops_alert(&key, &payload).await {
        Ok(true) => tracing::warn!(host, issues = report.issues.as_ref().map_or(0, Vec::len), "host baseline differs; ops_alert queued"),
        Ok(false) => {}
        Err(e) => tracing::error!(host, error = %e, "host baseline differs; ops_alert could not be queued"),
    }
}

impl<T: BaselineEnv + ?Sized> BaselineEnv for std::sync::Arc<T> {
    fn baseline_cache(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, BaselineReport>> {
        (**self).baseline_cache()
    }
    fn local_herdr_connected(&self) -> bool {
        (**self).local_herdr_connected()
    }
    fn push_ops_alert(&self, key: &str, payload: &serde_json::Value) -> impl std::future::Future<Output = anyhow::Result<bool>> + Send {
        (**self).push_ops_alert(key, payload)
    }
}

/// 定期重量的間隔：偵測本來只在連上、alias 變了、手動時才跑，設定被改掉（或補好）要等到下一次才看得到。
pub const RECHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// 每隔 [`RECHECK_EVERY`] 對每台連著的主機重跑一次偵測（含這份檢查）；啟動那一輪由開機偵測負責，所以先睡再做。
pub fn spawn_poller<H: crate::tools::ToolsEnv>(app: std::sync::Arc<H>) {
    crate::background_loop::spawn_periodic(&app, "host baseline recheck", RECHECK_EVERY, RECHECK_EVERY, |app| async move {
        for name in app.hosts().names().await {
            let connected = match app.hosts().get(&name).await {
                Some(c) if c.is_local() => app.local_herdr_connected(),
                Some(c) => c.is_connected(),
                None => false,
            };
            if connected {
                crate::tools::spawn_detect(app.clone(), name);
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



/// 每台主機的基線報告。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait HostBaselineTable: Send + Sync {
    fn host_baseline(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::host_baseline::BaselineReport>>;
}
