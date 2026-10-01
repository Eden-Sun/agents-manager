//! Issue #752 spike：claude 2.1.286 `--bare` 當成「專用短命 worker」的 opt-in 執行檔（runtime profile）。
//!
//! 這支只決定「若要用 `--bare`，哪些前提不滿足」與「該加的旗標」，是純函式；目前**沒有接到任何 bot 的啟動路徑**
//! （`injected_args`／`child_restart` 都不呼叫它），所以不會改動任何正式 bot 的啟動參數。
//! 旗標 `AGM_BARE_WORKER_CANARY=1` 沒開時 [`plan`] 一律回 `Normal`（無額外旗標）。
//!
//! 量測結論與「為什麼目前不採用」見 issue #752 的留言；前提檢查就是那份結論的程式化：
//! - `--bare` 不讀 OAuth／keychain，只吃 `ANTHROPIC_API_KEY`（或 `--settings` 的 apiKeyHelper）：AGM 的 claude 帳號
//!   都是 OAuth 訂閱（`CLAUDE_CONFIG_DIR`），沒有 API key 就是 `Not logged in`。
//! - `--bare` 連 `--settings` 注入的 hooks 都不跑（實測 SessionStart 0/10），daemon 的 hook 為主通道全斷，
//!   只剩終端快照備援；所以 `inject_hooks != 0` 的 bot 不相容。
//! - `--bare` 不載入使用者 skill（`/herdr` 補全找不到）：要開孫 pane 的 bot 不相容；這裡以 `needs_skills` 表達。

/// 執行檔：`Normal`＝今天的行為，`BareWorker`＝opt-in 低資源模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeProfile {
    #[default]
    Normal,
    BareWorker,
}

impl RuntimeProfile {
    /// 設定值 `runtime_profile = "bare-worker"`；其他（含空字串）都當 normal，不報錯：未知值不可讓 bot 起不來。
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "bare-worker" => RuntimeProfile::BareWorker,
            _ => RuntimeProfile::Normal,
        }
    }
}

/// 為什麼這顆 bot 不能（或不會）用 `--bare`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocker {
    /// 旗標 `AGM_BARE_WORKER_CANARY` 沒開。
    FlagOff,
    /// 只有 claude 有 `--bare`。
    NotClaude,
    /// 沒有 `ANTHROPIC_API_KEY`：`--bare` 不讀 OAuth。
    NoApiKey,
    /// `inject_hooks != 0`：`--bare` 不跑 hooks，daemon 收不到 SessionStart／Stop。
    NeedsHooks,
    /// bot 需要 user／project skill（例如 herdr）：`--bare` 不載入。
    NeedsSkills,
}

/// 一顆 bot 要不要用 `--bare` 的判定輸入。
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    pub profile: RuntimeProfile,
    pub kind: &'a str,
    pub inject_hooks: bool,
    pub needs_skills: bool,
    pub has_api_key: bool,
    pub flag_on: bool,
}

/// 判定結果：`args` 空＝照常啟動；`blockers` 非空時 `args` 一定是空（寧可 normal，也不要起一顆登不進去的 bot）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub args: Vec<String>,
    pub blockers: Vec<Blocker>,
}

/// 讀旗標：只有 `1`／`true` 算開。
pub fn flag_on(env_value: Option<&str>) -> bool {
    matches!(env_value.map(str::trim), Some("1") | Some("true"))
}

pub fn plan(i: Inputs<'_>) -> Plan {
    if i.profile == RuntimeProfile::Normal {
        return Plan { args: vec![], blockers: vec![] };
    }
    let mut blockers = Vec::new();
    if !i.flag_on {
        blockers.push(Blocker::FlagOff);
    }
    if i.kind != "claude" {
        blockers.push(Blocker::NotClaude);
    }
    if !i.has_api_key {
        blockers.push(Blocker::NoApiKey);
    }
    if i.inject_hooks {
        blockers.push(Blocker::NeedsHooks);
    }
    if i.needs_skills {
        blockers.push(Blocker::NeedsSkills);
    }
    let args = if blockers.is_empty() { vec!["--bare".to_string()] } else { vec![] };
    Plan { args, blockers }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_inputs() -> Inputs<'static> {
        Inputs {
            profile: RuntimeProfile::BareWorker,
            kind: "claude",
            inject_hooks: false,
            needs_skills: false,
            has_api_key: true,
            flag_on: true,
        }
    }

    #[test]
    fn parse_only_knows_bare_worker() {
        assert_eq!(RuntimeProfile::parse("bare-worker"), RuntimeProfile::BareWorker);
        assert_eq!(RuntimeProfile::parse(" bare-worker "), RuntimeProfile::BareWorker);
        assert_eq!(RuntimeProfile::parse(""), RuntimeProfile::Normal);
        assert_eq!(RuntimeProfile::parse("bare"), RuntimeProfile::Normal);
        assert_eq!(RuntimeProfile::default(), RuntimeProfile::Normal);
    }

    #[test]
    fn flag_defaults_off() {
        assert!(!flag_on(None));
        assert!(!flag_on(Some("")));
        assert!(!flag_on(Some("0")));
        assert!(flag_on(Some("1")));
        assert!(flag_on(Some("true")));
    }

    #[test]
    fn normal_profile_never_adds_args() {
        let p = plan(Inputs { profile: RuntimeProfile::Normal, ..ok_inputs() });
        assert_eq!(p, Plan { args: vec![], blockers: vec![] });
    }

    #[test]
    fn all_prerequisites_met_adds_bare() {
        let p = plan(ok_inputs());
        assert_eq!(p.args, vec!["--bare".to_string()]);
        assert!(p.blockers.is_empty());
    }

    #[test]
    fn flag_off_keeps_normal_even_when_everything_else_fits() {
        let p = plan(Inputs { flag_on: false, ..ok_inputs() });
        assert!(p.args.is_empty());
        assert_eq!(p.blockers, vec![Blocker::FlagOff]);
    }

    #[test]
    fn oauth_only_account_is_blocked() {
        let p = plan(Inputs { has_api_key: false, ..ok_inputs() });
        assert!(p.args.is_empty());
        assert_eq!(p.blockers, vec![Blocker::NoApiKey]);
    }

    #[test]
    fn hooked_bot_is_blocked_because_bare_skips_settings_hooks() {
        let p = plan(Inputs { inject_hooks: true, ..ok_inputs() });
        assert!(p.args.is_empty());
        assert_eq!(p.blockers, vec![Blocker::NeedsHooks]);
    }

    #[test]
    fn skill_dependent_bot_is_blocked() {
        let p = plan(Inputs { needs_skills: true, ..ok_inputs() });
        assert!(p.args.is_empty());
        assert_eq!(p.blockers, vec![Blocker::NeedsSkills]);
    }

    #[test]
    fn non_claude_kinds_are_blocked_and_blockers_accumulate() {
        let p = plan(Inputs { kind: "codex", inject_hooks: true, has_api_key: false, ..ok_inputs() });
        assert!(p.args.is_empty());
        assert_eq!(p.blockers, vec![Blocker::NotClaude, Blocker::NoApiKey, Blocker::NeedsHooks]);
    }

    /// 今天每一顆 AGM bot（OAuth、inject_hooks 預設 1、帶 herdr skill）都不相容：這是「目前不採用」的程式化版本。
    #[test]
    fn default_agm_claude_bot_is_not_eligible() {
        let p = plan(Inputs { inject_hooks: true, needs_skills: true, has_api_key: false, ..ok_inputs() });
        assert!(p.args.is_empty());
        assert_eq!(p.blockers, vec![Blocker::NoApiKey, Blocker::NeedsHooks, Blocker::NeedsSkills]);
    }
}
