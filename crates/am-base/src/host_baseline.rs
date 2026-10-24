//! 主機 CLI 工作環境一致性檢查（issue #719，SPEC §16.7）：**只讀、只報告，不修、不裝、不寫任何檔**。
//!
//! 跟 [`crate::tools::PROBE_SH`] 同一趟探測（同一次 ssh）：[`baseline_sh`] 接在它後面，印 `AM_BL …` 行；
//! [`evaluate`] 把這些行對照基準宣告檔 `ops/host-baseline/baseline.toml`（[`manifest`]），產生 `hosts[].baseline`。
//! 探測沒跑完（沒有結尾標記）就是 `None`（未知），不是「全部缺」——否則一次 ssh 逾時就會在每台主機上喊一排缺漏。

use serde::Serialize;

pub mod manifest;
pub mod plan;

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

    /// 對外投影（`hosts[].baseline`、`host_changed`、`tools/refresh`）：[`Self::snapshot`] 再加 `fixable`——
    /// dry-run 計畫（[`plan`]）裡可以自動補的差異 id。未知（`issues: None`）或宣告檔壞了＝空。
    pub fn view(&self, now: chrono::DateTime<chrono::Utc>) -> BaselineView {
        let report = self.snapshot(now);
        let fixable = match (report.issues.as_deref(), manifest::shipped(), manifest::shipped_values()) {
            (Some(issues), Ok(m), Ok(v)) => plan::build(m, v, issues).fixable(),
            _ => Vec::new(),
        };
        BaselineView { report, fixable }
    }
}

/// [`BaselineReport::view`] 的結果：報告本身攤平，加上 `fixable`。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BaselineView {
    #[serde(flatten)]
    pub report: BaselineReport,
    pub fixable: Vec<String>,
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

/// 只讀的探測腳本。接在 `PROBE_SH` 後面（不依賴它的函式）。身份＝`$CLAUDE_CONFIG_DIR`、`~/.claude`、`~/.claude-cc<N>`。
/// 清單由宣告檔代入（[`render_baseline_sh`]）；宣告檔壞掉（只會是沒跑過 `cargo test` 的 build）就回空字串：
/// 探測沒有 `AM_BL end`，[`evaluate`] 回 `None`（未知），不會變成空清單、悄悄跳過檢查，也不 panic。
pub fn baseline_sh() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| match manifest::shipped() {
        Ok(m) => render_baseline_sh(m),
        Err(e) => {
            tracing::error!(error = %e, "host baseline manifest is invalid; baseline reported as unknown");
            String::new()
        }
    })
}

/// 單引號包起來的清單（值已過 [`manifest`] 的白名單，這裡再整個 quote 一次）。
fn sh_words(items: impl IntoIterator<Item = String>) -> String {
    items.into_iter().map(|w| crate::hosts::sh_quote(&w)).collect::<Vec<_>>().join(" ")
}

/// 用宣告檔填 [`BASELINE_SH_TEMPLATE`] 的佔位字。
pub fn render_baseline_sh(m: &manifest::Manifest) -> String {
    let names = |v: &[manifest::Item]| v.iter().map(|i| i.name.clone()).collect::<Vec<_>>();
    // 工具清單另有一份放在 `$SHELL -lic '…'` 的單引號裡：先 quote 成內層 shell 的字，再把 `'` 寫成 `'\''` 嵌進外層單引號。
    let tools_inner = sh_words(names(&m.tools)).replace('\'', "'\\''");
    BASELINE_SH_TEMPLATE
        .replace("__AM_TOOLS_INNER__", &tools_inner)
        .replace("__AM_TOOLS__", &sh_words(names(&m.tools)))
        .replace("__AM_CLAUDE_FILES__", &sh_words(names(&m.claude_files)))
        .replace("__AM_CLAUDE_KEYS__", &sh_words(m.claude_keys.iter().map(|k| format!("{}={}", k.name, k.key))))
        .replace("__AM_MAC_ONLY_PLUGINS__", &sh_words(m.mac_only_plugins.iter().cloned()))
        .replace("__AM_CODEX_FILES__", &sh_words(names(&m.codex_files)))
        .replace(
            "__AM_CODEX_KEYS__",
            &sh_words(m.codex_keys.iter().map(|k| {
                let check = match k.check {
                    manifest::CodexCheck::Present => "present",
                    manifest::CodexCheck::True => "true",
                };
                format!("{check}:{}", k.key)
            })),
        )
        .replace("__AM_GROK_FILES__", &sh_words(names(&m.grok_files)))
        .replace("__AM_HERDR_FILES__", &sh_words(names(&m.herdr_files)))
}

const BASELINE_SH_TEMPLATE: &str = r#"
printf 'AM_BL begin\n'
printf 'AM_BL os %s\n' "$(uname -s 2>/dev/null)"
# 工具一次問完：每次 `$SHELL -lic` 都要讀完整個 rc（nvm／conda 動輒數秒），PROBE_SH 自己已經開了好幾次，
# 再各開一次會把整趟探測推過 ssh 的 30 秒上限，連原本的 tools 偵測都跟著失敗。只認絕對路徑（alias 的字串不算，#666）。
# HISTFILE=/dev/null：macOS /bin/sh 是 bash 3.2，-i 會把 history 寫進 $HOME。這支腳本必須只讀。
bl_paths=$( HISTFILE=/dev/null "${SHELL:-/bin/sh}" -lic 'for t in __AM_TOOLS_INNER__; do printf "AM_BLP %s %s\n" "$t" "$(command -v "$t" 2>/dev/null | tail -1)"; done' 2>/dev/null </dev/null | grep '^AM_BLP ' )
bl_jq_path=$(printf '%s\n' "$bl_paths" | sed -n 's/^AM_BLP jq //p' | tail -1)
case "$bl_jq_path" in /*) [ -x "$bl_jq_path" ] || bl_jq_path="" ;; *) bl_jq_path=$(command -v jq 2>/dev/null || true) ;; esac
for t in __AM_TOOLS__; do
  p=$(printf '%s\n' "$bl_paths" | sed -n "s/^AM_BLP $t //p" | tail -1)
  case "$p" in /*) ;; *) p="" ;; esac
  [ -n "$p" ] || p=$(command -v "$t" 2>/dev/null)
  case "$p" in /*) ;; *) p="" ;; esac
  printf 'AM_BL tool %s %s\n' "$t" "$p"
done
bl_has() { [ -e "$1" ] && printf 1 || printf 0; }
bl_plugin() { [ -f "$1" ] && grep -Eq "\"$2@[^\"]*\"[[:space:]]*:[[:space:]]*true" "$1" 2>/dev/null && printf 1 || printf 0; }
# 點號路徑（`permissions.defaultMode`）的每一段都要是物件裡的鍵。
bl_key() {
  [ -f "$1" ] || { printf 0; return; }
  [ -n "$bl_jq_path" ] && [ -x "$bl_jq_path" ] || { printf '?'; return; }
  # jq 退出碼：0＝有、1＝沒有，其他（5＝不是合法 JSON）＝x。讀不懂不能算成沒有那個鍵。
  "$bl_jq_path" -e --arg p "$2" 'def bl_h($ks): if ($ks | length) == 0 then true elif type == "object" and has($ks[0]) then (.[$ks[0]] | bl_h($ks[1:])) else false end; bl_h($p | split("."))' "$1" >/dev/null 2>&1
  case $? in 0) printf 1 ;; 1) printf 0 ;; *) printf x ;; esac
}
# TOML 裡有 `<鍵> =`（任何值、任何表）。不開 login shell。
bl_toml_has() {
  [ -f "$1" ] && grep -Eq "^[[:space:]]*$2[[:space:]]*=" "$1" 2>/dev/null && printf 1 || printf 0
}
# `<表>.<鍵> = true`，或 `[<表>]` 表裡的 `<鍵> = true`。false／沒寫＝0。不開 login shell。
bl_toml_true() {
  [ -f "$1" ] || { printf 0; return; }
  /usr/bin/awk -v t="$2" -v k="$3" '
    BEGIN { root = 1 }
    {
      line = $0
      sub(/\r$/, "", line)
      sub(/^[[:blank:]]*/, "", line)
      if (line == "" || line ~ /^#/) next
      if (line ~ /^\[/) {
        in_table = (line ~ ("^\\[" t "\\][[:blank:]]*(#.*)?$"))
        root = 0
        next
      }
      sub(/[[:blank:]]+#.*/, "", line)
      if ((root && line ~ ("^" t "\\." k "[[:blank:]]*=[[:blank:]]*true[[:blank:]]*$")) ||
          (in_table && line ~ ("^" k "[[:blank:]]*=[[:blank:]]*true[[:blank:]]*$"))) found = 1
    }
    END { exit !found }
  ' "$1" && printf 1 || printf 0
}
# 同一個目錄只查一次。身份名只認一般字元；路徑不印出來。
bl_seen="|"
bl_claude() {
  d=$1
  n=$2
  case "$n" in *[!A-Za-z0-9_.-]*) return ;; esac
  [ -n "$d" ] && [ -d "$d" ] || return
  case "$bl_seen" in *"|$d|"*) return ;; esac
  bl_seen="${bl_seen}${d}|"
  printf 'AM_BL claude-dir %s\n' "$n"
  for f in __AM_CLAUDE_FILES__; do
    printf 'AM_BL claude-file %s %s %s\n' "$n" "$f" "$(bl_has "$d/$f")"
  done
  for k in __AM_CLAUDE_KEYS__; do
    printf 'AM_BL claude-key %s %s %s\n' "$n" "${k%%=*}" "$(bl_key "$d/settings.json" "${k#*=}")"
  done
  for pl in __AM_MAC_ONLY_PLUGINS__; do
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
for f in __AM_CODEX_FILES__; do
  printf 'AM_BL codex-file %s %s\n' "$f" "$(bl_has "$CX/$f")"
done
for bl_k in __AM_CODEX_KEYS__; do
  case "$bl_k" in
    present:*) bl_v=$(bl_toml_has "$CX/config.toml" "${bl_k#present:}") ;;
    true:*) bl_n=${bl_k#true:}; bl_v=$(bl_toml_true "$CX/config.toml" "${bl_n%%.*}" "${bl_n#*.}") ;;
    *) continue ;;
  esac
  printf 'AM_BL codex-key %s %s\n' "${bl_k#*:}" "$bl_v"
done
for f in __AM_GROK_FILES__; do
  printf 'AM_BL grok-file %s %s\n' "$f" "$(bl_has "${GROK_HOME:-$HOME/.grok}/$f")"
done
for f in __AM_HERDR_FILES__; do
  printf 'AM_BL herdr-file %s %s\n' "$f" "$(bl_has "${XDG_CONFIG_HOME:-$HOME/.config}/herdr/$f")"
done
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

/// 宣告檔裡的嚴重度；探測印出宣告檔沒列的名稱（舊腳本）時當提醒。
fn severity_of(table: &[manifest::Item], name: &str) -> &'static str {
    manifest::severity_of(table, name).unwrap_or(WARN)
}

/// 解 [`BASELINE_SH`] 的輸出，照探測印出的順序列出缺漏。沒有 `AM_BL end`（探測被截斷、沒跑到）＝`None`。
///
/// 檔案本身不在時不再列裡面的鍵（`settings.json` 沒有就不用再說 `statusLine`、`hooks` 沒有）：一個根因一行。
pub fn evaluate(out: &str) -> Option<Vec<BaselineIssue>> {
    if !out.lines().any(|l| l.trim_end() == "AM_BL end") {
        return None;
    }
    // 宣告檔壞掉：不知道基準是什麼，就是未知（不是一致）。
    let m = manifest::shipped().ok()?;
    let mac = os_of(out).as_deref() == Some("Darwin");
    let mut issues = Vec::new();
    // 目前這個 claude 身份的 settings.json 在不在、codex 的 config.toml 在不在：決定要不要看裡面的鍵。
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
                if f.get(3).map_or(true, |p| p.is_empty()) {
                    if let Some(sev) = manifest::severity_of(&m.tools, f[2]) {
                        issues.push(issue(format!("tool.{}", f[2]), sev, format!("缺工具 {}（登入 shell 的 PATH 找不到）", f[2])));
                    }
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
                        severity_of(&m.claude_files, f[3]),
                        format!("claude 身份 {}：缺 {}", f[2], f[3]),
                    ));
                }
            }
            "claude-key" if f.len() >= 5 => {
                // `x`：jq 讀不懂整份檔。那不是「沒有這個鍵」，只報一條（四個鍵的 id 同一個，下面去重只留一條）。
                if claude_settings && f.get(4).copied() == Some("x") {
                    issues.push(issue(
                        format!("claude.{}.settings.json:json", f[2]),
                        CRITICAL,
                        format!("claude 身份 {}：settings.json 不是合法 JSON，裡面有沒有那些鍵看不出來", f[2]),
                    ));
                } else if claude_settings && f.get(4).copied() != Some("?") && !present(f.get(4)) {
                    issues.push(issue(
                        format!("claude.{}.settings.json:{}", f[2], f[3]),
                        m.claude_keys.iter().find(|k| k.name == f[3]).map_or(WARN, |k| k.severity),
                        format!("claude 身份 {}：settings.json 沒有 {}", f[2], f[3]),
                    ));
                }
            }
            "claude-plugin" if f.len() >= 5 => {
                // Mac 專用：Linux（或不知道是什麼系統）一律不算缺；settings.json 本身不在也不再追問。
                if mac && claude_settings && m.mac_only_plugins.iter().any(|p| p == f[3]) && !present(f.get(4)) {
                    issues.push(issue(
                        format!("claude.{}.plugin:{}", f[2], f[3]),
                        WARN,
                        format!("claude 身份 {}：沒有啟用 {} plugin（Mac 專用）", f[2], f[3]),
                    ));
                }
            }
            "codex-file" if f.len() >= 4 => {
                let present = present(f.get(3));
                if f[2] == "config.toml" {
                    codex_config = present;
                }
                if !present {
                    issues.push(issue(format!("codex.{}", f[2]), severity_of(&m.codex_files, f[2]), format!("codex：缺 {}", f[2])));
                }
            }
            "codex-key" if f.len() >= 4 => {
                if codex_config && !present(f.get(3)) {
                    issues.push(issue(
                        format!("codex.config.toml:{}", f[2]),
                        m.codex_keys.iter().find(|k| k.key == f[2]).map_or(WARN, |k| k.severity),
                        format!("codex：config.toml 沒有 {}", f[2]),
                    ));
                }
            }
            "grok-file" | "herdr-file" if f.len() >= 4 => {
                if !present(f.get(3)) {
                    let who = f[1].trim_end_matches("-file");
                    let table = if who == "grok" { &m.grok_files } else { &m.herdr_files };
                    issues.push(issue(format!("{who}.{}", f[2]), severity_of(table, f[2]), format!("{who}：缺 {}", f[2])));
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
