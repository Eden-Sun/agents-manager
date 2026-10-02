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
    /// 探測沒跑完＝`None`（未知）；跑完而且一致＝`Some([])`。
    pub issues: Option<Vec<BaselineIssue>>,
    pub checked_at: String,
}

/// 只讀。接在 `PROBE_SH` 後面（用它定義的 `am_abs`）。身分＝`$CLAUDE_CONFIG_DIR`、`~/.claude`、`~/.claude-cc<N>`。
pub const BASELINE_SH: &str = r#"
printf 'AM_BL begin\n'
for t in herdr rtk zsh bun jq gh; do
  printf 'AM_BL tool %s %s\n' "$t" "$(am_abs "$t")"
done
bl_has() { [ -e "$1" ] && printf 1 || printf 0; }
bl_key() { [ -f "$1" ] && grep -Eq "\"$2\"[[:space:]]*:" "$1" 2>/dev/null && printf 1 || printf 0; }
for d in "${CLAUDE_CONFIG_DIR:-}" "$HOME/.claude" "$HOME"/.claude-cc[0-9]*; do
  [ -n "$d" ] && [ -d "$d" ] || continue
  case "$d" in "$HOME/.claude") n=default ;; "$HOME"/.claude-cc*) n=${d##*/.claude-} ;; *) n=custom ;; esac
  printf 'AM_BL claude-dir %s %s\n' "$n" "$d"
  for f in settings.json statusline-command.sh CLAUDE.md RTK.md; do
    printf 'AM_BL claude-file %s %s %s\n' "$n" "$f" "$(bl_has "$d/$f")"
  done
  for k in statusLine hooks permissions; do
    printf 'AM_BL claude-key %s %s %s\n' "$n" "$k" "$(bl_key "$d/settings.json" "$k")"
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
if [ -f "$HOME/.gitconfig" ] && grep -Eq 'https?://[^/@[:space:]]+:[^/@[:space:]]+@' "$HOME/.gitconfig" 2>/dev/null; then
  printf 'AM_BL gitconfig-token 1\n'
else
  printf 'AM_BL gitconfig-token 0\n'
fi
printf 'AM_BL end\n'
"#;

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
    Some(issues)
}

#[cfg(test)]
mod tests;
