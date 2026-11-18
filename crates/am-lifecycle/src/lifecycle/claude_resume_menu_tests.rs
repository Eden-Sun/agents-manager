use super::*;
use crate::tui_prompts::{cold_resume_choice, ResumeChoice};
// 真實 2.1.304 PTY 畫面，2026-10-20 重新擷取；獨立 binary 副本、拋棄式 cwd/session、safe-mode。
// Max 5x / Opus 4.6[1m]；只修改自己的 transcript timestamp（2h）與 usage/iterations（1M）。
// 沒有自然等待 2h，也沒有真的發送 1M tokens。畫面由 @xterm/headless 80x24 從 script(1) raw stream 還原。
const ACTUAL: &str = include_str!("fixtures/claude-2.1.304-cold-resume.txt");
// 尚未實機重現：依 2.1.304 binary 的固定字串與無編號選項合成；帳號 idle 功能旗標未開。
const SYNTHETIC: &str = include_str!("fixtures/claude-2.1.304-cold-resume-unsent-synthetic.txt");
#[test]
fn cold_resume_actual_is_not_ready_an_error_a_reply_or_a_human_question() {
    assert_eq!(cold_resume_choice(ACTUAL), Some(ResumeChoice::Resume));
    for reported in [AgentStatus::Idle, AgentStatus::Blocked] {
        assert_eq!(
            startup_status("claude", ACTUAL, reported),
            AgentStatus::Unknown
        );
    }
    assert_eq!(
        super::super::delivery::box_state("claude", ACTUAL),
        crate::composer_parse::BoxState::Unready
    );
    assert!(super::super::screen::clean_screen("claude", ACTUAL).is_none());
    assert!(super::super::screen::extract_reply("claude", ACTUAL).is_none());
    assert!(crate::turn_error::api_error_line(ACTUAL).is_none());
    assert!(!crate::tui_prompts::awaits_menu_choice(ACTUAL));
    assert!(crate::tui_prompts::permission_prompt(ACTUAL).is_none());
    assert!(crate::child_alerts::alertable_question(ACTUAL).is_none());
}
#[test]
fn cold_resume_synthetic_unsent_is_recognized_without_numbered_options() {
    assert_eq!(
        cold_resume_choice(SYNTHETIC),
        Some(ResumeChoice::NewConversation)
    );
    assert!(SYNTHETIC.contains("Your unsent message will still be there after you answer"));
    assert_eq!(
        startup_status("claude", SYNTHETIC, AgentStatus::Idle),
        AgentStatus::Unknown
    );
    assert_eq!(
        super::super::delivery::box_state("claude", SYNTHETIC),
        crate::composer_parse::BoxState::Unready
    );
    assert!(crate::turn_error::api_error_line(SYNTHETIC).is_none());
    assert!(!crate::tui_prompts::awaits_menu_choice(SYNTHETIC));
    assert!(crate::child_alerts::alertable_question(SYNTHETIC).is_none());
}
#[test]
fn cold_resume_other_resume_screens_and_incomplete_dialogs_never_authorize_keys() {
    for screen in [
        "Resume",
        "❯ Resume\n  Start a new conversation\nEnter to confirm",
        "Resume this conversation?\n❯ Resume\nStart a new conversation",
    ] {
        assert_eq!(cold_resume_choice(screen), None, "{screen}");
    }
    for screen in [
        ACTUAL.replace("of your 5-hour usage limit", "of the cost"),
        ACTUAL.replace("1. Resume", "1. Retry"),
        ACTUAL.replace("Start a new conversation", "Cancel"),
        format!("{ACTUAL}\n────────────────\n❯\n────────────────\n"),
        format!("{ACTUAL}\n● This is just a quoted example.\n"),
        format!("{ACTUAL}\n────────────────\n❯ a user's draft\n────────────────\n"),
    ] {
        assert_eq!(cold_resume_choice(&screen), None, "{screen}");
    }
}
#[test]
fn cold_resume_synthetic_all_of_limit_and_wrapped_warning_are_supported() {
    let all = SYNTHETIC.replace("about 10%", "all");
    assert_eq!(
        cold_resume_choice(&all),
        Some(ResumeChoice::NewConversation)
    );
    let wrapped = SYNTHETIC.replace(
        "of your 5-hour usage limit",
        "of your\n  5-hour usage limit",
    );
    assert_eq!(
        cold_resume_choice(&wrapped),
        Some(ResumeChoice::NewConversation)
    );
    assert_eq!(
        startup_status("codex", ACTUAL, AgentStatus::Idle),
        AgentStatus::Idle
    );
}
#[test]
fn cold_resume_warn_table_writes_each_reason_once_per_run() {
    let t0 = std::time::Instant::now();
    let mut t = GiveUps::default();
    assert!(t.first_time("run-a", CURSOR_NOT_ON_RESUME, t0));
    assert!(!t.first_time("run-a", CURSOR_NOT_ON_RESUME, t0), "同一原因不重複寫");
    assert!(t.first_time("run-b", CURSOR_NOT_ON_RESUME, t0), "別的 run 各自計");
    assert!(t.first_time("run-a", SCREEN_CHANGED, t0), "原因變了才再寫");
    assert!(!t.first_time("run-a", SCREEN_CHANGED, t0));
    assert!(!t.first_time("run-a", CURSOR_NOT_ON_RESUME, t0), "A→B→A：第二次 A 不再寫");
    assert!(!t.first_time("run-a", SCREEN_CHANGED, t0), "A→B→A→B 也不寫");
}
#[test]
fn cold_resume_warn_table_evicts_the_oldest_entry_at_the_cap() {
    let t0 = std::time::Instant::now();
    let at = |i: usize| t0 + Duration::from_secs(i as u64);
    let mut t = GiveUps::default();
    for i in 0..GIVE_UP_CAP {
        assert!(t.first_time(&format!("run-{i}"), STILL_SHOWN, at(i)));
    }
    assert_eq!(t.0.len(), GIVE_UP_CAP);
    assert!(t.first_time("run-new", STILL_SHOWN, at(GIVE_UP_CAP)));
    assert_eq!(t.0.len(), GIVE_UP_CAP, "不超過上限");
    let has = |t: &GiveUps, id: &str| t.0.contains_key(&(id.to_string(), STILL_SHOWN));
    assert!(!has(&t, "run-0"), "插入時間最舊的被淘汰");
    assert!(has(&t, "run-1") && has(&t, "run-new"));
    assert!(t.first_time("run-0", STILL_SHOWN, at(GIVE_UP_CAP + 1)), "被淘汰的下次當作第一次");
}
#[test]
fn cold_resume_warn_table_entry_expires_after_the_ttl_and_warns_again() {
    let t0 = std::time::Instant::now();
    let mut t = GiveUps::default();
    assert!(t.first_time("run-a", CURSOR_NOT_ON_RESUME, t0));
    let almost = t0 + GIVE_UP_TTL;
    assert!(!t.first_time("run-a", CURSOR_NOT_ON_RESUME, almost), "剛好 TTL 還算有效");
    let later = t0 + GIVE_UP_TTL + Duration::from_secs(1);
    assert!(t.first_time("run-a", CURSOR_NOT_ON_RESUME, later), "過期後同一原因再印一次");
    assert_eq!(t.0.len(), 1, "過期那筆被刪掉再重新記");
    assert!(!t.first_time("run-a", CURSOR_NOT_ON_RESUME, later + Duration::from_secs(1)));
}
