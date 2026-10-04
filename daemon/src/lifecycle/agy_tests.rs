//! agy 的 hook／設定檔安裝、dispatcher、送達證據與輸入框判讀（SPEC §agy）。
//! 寫設定檔的測試一律用拋棄式家目錄（`install_at`）或測試行程的假 HOME（`crate::home::dir()`），絕不碰真的 `~/.gemini`。

use super::agy_hook::{dispatch_sh, install_at, install_local};
use crate::agy_support as cfg;
use crate::testing as tt;
use serde_json::{json, Value};
use std::path::Path;

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[tokio::test]
async fn install_merges_only_our_entries_into_the_users_files_atomically() {
    let e = tt::env().await;
    let home = tt::scratch_dir("am-agy-home");
    let hooks = cfg::hooks_path(&home);
    let settings = cfg::settings_path(&home);
    std::fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&hooks, r#"{"their-tool":{"enabled":true,"Stop":[{"type":"command","command":"/x/stop.sh"}]}}"#).unwrap();
    std::fs::write(&settings, r#"{"colorScheme":"dark","trustedWorkspaces":["/already"],"permissions":{"allow":["command(git)"]}}"#).unwrap();

    assert!(install_at(&e.app, &home).unwrap(), "first install writes");
    let h = read_json(&hooks);
    assert_eq!(h["their-tool"]["Stop"][0]["command"], "/x/stop.sh", "別的工具的 hook 原封不動");
    let dispatcher = e.app.data_dir.join(cfg::DISPATCH_SH);
    for event in cfg::HOOK_EVENTS {
        let cmd = h["agents-manager"][event][0]["command"].as_str().unwrap();
        assert!(cmd.contains(dispatcher.to_str().unwrap()) && cmd.ends_with(event), "{cmd}");
    }
    let s = read_json(&settings);
    assert_eq!(s["colorScheme"], "dark");
    assert_eq!(s["trustedWorkspaces"], json!(["/already"]), "信任清單不是 install 的事");
    assert_eq!(s["permissions"]["allow"], json!(["command(git)"]));
    assert!(s["statusLine"]["command"].as_str().unwrap().ends_with(" state"));
    assert_eq!(s["statusLine"]["stack_with_default"], true);
    let leftovers: Vec<_> = std::fs::read_dir(hooks.parent().unwrap()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(leftovers, ["hooks.json"], "原子寫入不留暫存檔：{leftovers:?}");
    assert!(!install_at(&e.app, &home).unwrap(), "第二次什麼都不用改");
}

#[tokio::test]
async fn install_leaves_a_users_own_status_line_and_unreadable_files_alone() {
    let e = tt::env().await;
    let home = tt::scratch_dir("am-agy-home-own");
    let settings = cfg::settings_path(&home);
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let mine = r#"{"statusLine":{"type":"command","command":"/home/me/bar.sh"}}"#;
    std::fs::write(&settings, mine).unwrap();
    install_at(&e.app, &home).unwrap();
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), mine, "使用者自己的 statusLine 不碰（連排版都不動）");

    let broken_home = tt::scratch_dir("am-agy-home-broken");
    let hooks = cfg::hooks_path(&broken_home);
    std::fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    std::fs::write(&hooks, "{ not json").unwrap();
    assert!(install_at(&e.app, &broken_home).is_err(), "讀不懂就回錯");
    assert_eq!(std::fs::read_to_string(&hooks).unwrap(), "{ not json", "而且不覆寫");
}

/// 測試行程的假 HOME 才是 `install_local` 寫的地方（真的 HOME 一個字都不會動）。
#[tokio::test]
async fn install_local_writes_under_the_fake_home_only() {
    let e = tt::env().await;
    install_local(&e.app).unwrap();
    let home = crate::home::dir().unwrap();
    assert!(home.file_name().unwrap().to_string_lossy().starts_with("am-test-home-"), "{}", home.display());
    assert!(cfg::hooks_path(&home).is_file() && cfg::settings_path(&home).is_file());
    if let Some(real) = std::env::var_os("AM_TEST_REAL_HOME").map(std::path::PathBuf::from) {
        assert!(!cfg::hooks_path(&real).exists());
    }
}

fn run_dispatcher(script: &str, args: &[&str], env: &[(&str, &str)]) -> (String, i32) {
    let dir = tt::scratch_dir("am-agy-dispatch");
    let sh = dir.join("agy-hook.sh");
    tt::write_exec(&sh, script);
    let out = crate::exec_retry::output(std::process::Command::new(&sh).args(args).env_clear().env("PATH", "/usr/bin:/bin").envs(env.iter().copied())).unwrap();
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.code().unwrap_or(-1))
}

/// hook 事件的 stdout 會被 agy 當成決策：永遠只能是 `{}`；statusLine 的 stdout 是狀態列文字：什麼都不印。exit 0。
#[test]
fn the_dispatcher_always_answers_empty_json_for_hooks_nothing_for_the_status_line_and_exits_zero() {
    let stub = tt::scratch_dir("am-agy-stub").join("daemon");
    tt::write_exec(&stub, "#!/bin/sh\necho \"$@\" >> \"$AM_TEST_LOG\"\necho SHOULD-NOT-REACH-AGY-STDOUT\nexit 3\n");
    let log = stub.with_file_name("calls.log");
    let script = dispatch_sh(stub.to_str().unwrap(), "/data dir", None);
    let env = [("AM_BOT_ID", "bot1"), ("AM_HOOK_TOKEN", "tok"), ("AM_PORT", "7791"), ("AM_TEST_LOG", log.to_str().unwrap())];

    let (out, code) = run_dispatcher(&script, &["Stop"], &env);
    assert_eq!((out.as_str(), code), ("{}\n", 0), "daemon 的 stdout 不能漏給 agy，失敗也照樣 exit 0");
    let (out, code) = run_dispatcher(&script, &["state"], &env);
    assert_eq!((out.as_str(), code), ("", 0), "statusLine 什麼都不印");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("hook agy --bot bot1 --port 7791 --data-dir /data dir --event Stop"), "{calls}");
    assert!(calls.contains("--event state"), "{calls}");

    // 不是 AG Man 的 pane（沒有 bot env）：什麼都不做，使用者自己開的 agy 不受影響。
    std::fs::remove_file(&log).unwrap();
    let (out, code) = run_dispatcher(&script, &["PreInvocation"], &[("AM_TEST_LOG", log.to_str().unwrap())]);
    assert_eq!((out.as_str(), code), ("{}\n", 0));
    assert!(!log.exists(), "沒有 AM_BOT_ID／AM_HOOK_TOKEN：不呼叫 daemon");
}

#[test]
fn the_dispatcher_only_serves_its_own_daemon_instance() {
    let stub = tt::scratch_dir("am-agy-stub-inst").join("daemon");
    tt::write_exec(&stub, "#!/bin/sh\necho x >> \"$AM_TEST_LOG\"\n");
    let log = stub.with_file_name("calls.log");
    let base = [("AM_BOT_ID", "b"), ("AM_HOOK_TOKEN", "t"), ("AM_TEST_LOG", log.to_str().unwrap())];
    let prod = dispatch_sh(stub.to_str().unwrap(), "/d", None);
    let dev = dispatch_sh(stub.to_str().unwrap(), "/d", Some("dev"));
    run_dispatcher(&prod, &["Stop"], &[&base[..], &[("AM_INSTANCE", "dev")]].concat());
    run_dispatcher(&dev, &["Stop"], &base);
    assert!(!log.exists(), "別顆 daemon 實例的 pane 不處理");
    run_dispatcher(&prod, &["Stop"], &base);
    run_dispatcher(&dev, &["Stop"], &[&base[..], &[("AM_INSTANCE", "dev")]].concat());
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);
}

mod delivery_and_screen {
    use crate::lifecycle::delivery::{box_state, choose_proof, log_hits_since, BoxState, LogFormat, Proof, ProofInputs};
    use crate::testing as tt;

    fn inputs<'a>(session: Option<&'a str>, path: Option<&'a str>, local: bool) -> ProofInputs<'a> {
        ProofInputs { kind: "agy", host_is_local: local, hooks: true, session_id: session, transcript_path: path, codex_log: None, waited_for_log: false, pane_cols: Some(100) }
    }

    const USER: &str = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>\nsay OK\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>"}"#;

    #[test]
    fn the_first_prompt_has_no_transcript_to_prove_it_but_later_ones_do() {
        // 對話是第一則 prompt 才建立：還沒有 session／transcript → 照打照送、記成未驗證，不擋也不重送。
        assert_eq!(choose_proof(&inputs(None, None, true), "say OK").unwrap(), Proof::Unverified);
        let dir = tt::scratch_dir("am-agy-proof");
        let t = dir.join("transcript_full.jsonl");
        std::fs::write(&t, format!("{USER}\n")).unwrap();
        let p = choose_proof(&inputs(Some("c-1"), Some(t.to_str().unwrap()), true), "say OK").unwrap();
        assert_eq!(p, Proof::Transcript { format: LogFormat::Agy, path: t.clone(), session_id: "c-1".into() });
        // 檔案不在（路徑是占位）／遠端：沒有無損證據。
        assert_eq!(choose_proof(&inputs(Some("c-1"), Some("/nope/transcript_full.jsonl"), true), "x").unwrap(), Proof::Unverified);
        assert_eq!(choose_proof(&inputs(Some("c-1"), Some(t.to_str().unwrap()), false), "x").unwrap(), Proof::Unverified);
    }

    #[test]
    fn a_user_input_step_in_the_transcript_proves_the_exact_prompt_after_the_baseline() {
        let dir = tt::scratch_dir("am-agy-hits");
        let t = dir.join("transcript_full.jsonl");
        std::fs::write(&t, format!("{USER}\n")).unwrap();
        let baseline = std::fs::metadata(&t).unwrap().len();
        assert_eq!(log_hits_since(LogFormat::Agy, &t, baseline, "say OK").unwrap(), 0, "基準之前的不算");
        let mut f = std::fs::OpenOptions::new().append(true).open(&t).unwrap();
        use std::io::Write as _;
        writeln!(f, "{USER}").unwrap();
        writeln!(f, "{}", USER.replace("say OK", "say NO")).unwrap();
        assert_eq!(log_hits_since(LogFormat::Agy, &t, baseline, "say OK").unwrap(), 1);
        assert_eq!(log_hits_since(LogFormat::Agy, &t, baseline, "  say OK\n").unwrap(), 1, "agy 把原文夾在標籤裡、頭尾空白不算差異");
        assert_eq!(log_hits_since(LogFormat::Agy, &t, baseline, "say").unwrap(), 0, "要整句一字不差");
    }

    const RULE: &str = "────────────────────────────────────────";
    fn screen(composer: &str) -> String {
        format!(" Antigravity CLI 1.2.16\n Gemini 3.1 Pro (Low)\n ~/p\n\n{RULE}\n{composer}\n{RULE}\n")
    }

    #[test]
    fn the_agy_composer_is_the_gt_row_between_two_rules() {
        assert_eq!(box_state("agy", &screen(">")), BoxState::Empty);
        assert_eq!(box_state("agy", &screen("> ")), BoxState::Empty, "尾端空白是 padding");
        assert_eq!(box_state("agy", &screen("> half a thought")), BoxState::NonEmpty);
        assert_eq!(box_state("agy", "no composer here\n"), BoxState::Unready);
        // 純文字讀不放寬佔位字（分不出是提示還是真的打了）：寧可 NonEmpty。
        assert_eq!(box_state("agy", &screen("> Ask anything")), BoxState::NonEmpty);
    }

    #[test]
    fn a_dim_placeholder_in_the_agy_composer_counts_as_empty_on_a_styled_read() {
        let styled = screen("\u{1b}[1m>\u{1b}[0m \u{1b}[2mAsk anything\u{1b}[0m");
        assert_eq!(box_state("agy", &styled), BoxState::Empty);
        let typed = screen("\u{1b}[1m>\u{1b}[0m hello");
        assert_eq!(box_state("agy", &typed), BoxState::NonEmpty);
    }
}

#[tokio::test]
async fn the_agy_transcript_root_is_the_brain_dir_under_the_home() {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "agy-roots").await;
    sqlx::query("UPDATE bots SET kind='agy' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
    let bot = crate::db::bot(&e.app.db, &bot.id).await.unwrap().unwrap();
    let roots = crate::transcript_read::trusted_roots(&e.app, &bot).await;
    assert_eq!(roots, [crate::home::dir().unwrap().join(".gemini/antigravity-cli/brain")]);
}

mod blocked_screens {
    use crate::testing as tt;

    /// herdr 判 idle 的登入框：補標 blocked、不送任何鍵、原因是 agy 對話框；框關掉就還原。
    #[tokio::test]
    async fn an_idle_agy_login_screen_is_marked_blocked_and_released_without_a_key() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "agy-login").await;
        sqlx::query("UPDATE bots SET kind='agy' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let run_id = tt::fake_run(&app, &bot.id).await;
        let run_of = || async { sqlx::query_as::<_, crate::db::Run>("SELECT * FROM runs WHERE id=?").bind(&run_id).fetch_one(&app.db).await.unwrap() };
        let login = "Welcome to the Antigravity CLI. You are currently not signed in.\n\nSelect login method:\n\n> 1. Google OAuth\n  2. Use a Google Cloud project\n";

        crate::session_paused::observe_agy_screen(&app, &run_of().await, login).await;
        assert_eq!(run_of().await.agent_status, "blocked");
        let reason = crate::blocked_reason::of(&run_id).expect("a reason for the blocked run");
        assert_eq!(reason.code, "agy_dialog");
        assert!(reason.text.contains("登入"), "{}", reason.text);

        crate::session_paused::observe_agy_screen(&app, &run_of().await, "> \n").await;
        assert_eq!(run_of().await.agent_status, "idle", "框關掉：還原成補標前的狀態");
        assert!(crate::blocked_reason::of(&run_id).is_none());
        assert!(e.herdr.calls_to("pane.send_keys").is_empty() && e.herdr.calls_to("pane.send_text").is_empty(), "一個鍵都不按");
    }
}
