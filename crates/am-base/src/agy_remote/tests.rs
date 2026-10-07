use super::*;
use crate::config::HostCfg;
use crate::testing as tt;
use serde_json::{json, Value};
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const ROOT: &str = crate::startup::REMOTE_ROOT;

// ── dispatcher：真的用 /bin/sh 跑（假 HOME、假 hook.sh／真 hook.sh）─────────────────────────────────

struct Sandbox {
    home: PathBuf,
    bot: String,
}

impl Sandbox {
    fn new() -> Self {
        let home = tt::scratch_dir("am-agy-remote");
        let bot = "b-test".to_string();
        std::fs::create_dir_all(home.join(ROOT).join("bots").join(&bot)).unwrap();
        let s = Sandbox { home, bot };
        write_exec(&s.home.join(ROOT).join(cfg::DISPATCH_SH), &dispatch_sh(ROOT, None));
        s
    }

    fn bot_dir(&self) -> PathBuf {
        self.home.join(ROOT).join("bots").join(&self.bot)
    }

    /// 真的遠端 `hook.sh`（spool 與 python 正規化都是正式碼）。
    fn real_hook(&self) {
        write_exec(&self.bot_dir().join("hook.sh"), &crate::lifecycle::remote_hook_sh(ROOT));
    }

    /// 假的 `hook.sh`：只把收到的參數與 stdin 記下來，看 dispatcher 交了什麼。
    fn fake_hook(&self) {
        write_exec(
            &self.bot_dir().join("hook.sh"),
            &format!("#!/bin/sh\necho \"$@\" > {d}/argv\ncat > {d}/stdin\n", d = self.home.display()),
        );
    }

    fn run(&self, event: &str, stdin: &str, env: &[(&str, &str)], path: &str) -> (String, bool) {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(self.home.join(ROOT).join(cfg::DISPATCH_SH)).arg(event);
        cmd.env_clear().env("PATH", path).env("HOME", &self.home);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut ch = cmd.spawn().unwrap();
        ch.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let out = ch.wait_with_output().unwrap();
        (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success())
    }

    fn pane_env(&self) -> Vec<(&'static str, String)> {
        vec![("AM_BOT_ID", self.bot.clone()), ("AM_HOOK_TOKEN", "tok".into())]
    }

    fn run_in_pane(&self, event: &str, stdin: &str, path: &str) -> (String, bool) {
        let env = self.pane_env();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        self.run(event, stdin, &env, path)
    }

    fn spooled(&self) -> Vec<Value> {
        let mut files: Vec<_> = std::fs::read_dir(self.bot_dir().join("hook-spool.d")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
        files.sort();
        files.iter().map(|p| serde_json::from_str(std::fs::read_to_string(p).unwrap().trim()).unwrap()).collect()
    }
}

fn write_exec(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

const SYS_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

fn has_python3() -> bool {
    Command::new("/bin/sh").arg("-c").arg("command -v python3").env("PATH", SYS_PATH).output().is_ok_and(|o| o.status.success())
}

/// 真機 1.2.16 的形狀（SPEC 附錄 G.5）：使用者輸入、工具步驟、最後給使用者的回覆、半行殘缺。
fn transcript() -> String {
    [
        json!({"step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE", "content": "<USER_REQUEST>\n舊的一句\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>"}),
        json!({"step_index": 1, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE", "input_tokens": 100, "content": "舊的回覆"}),
        json!({"step_index": 2, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE", "content": "<USER_REQUEST>\n現在部 \"demo\"\n第二行\n</USER_REQUEST>"}),
        json!({"step_index": 3, "source": "MODEL", "type": "RUN_COMMAND", "status": "DONE", "content": "ls"}),
        json!({"step_index": 4, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE", "input_tokens": 11824, "content": "好了 ✓"}),
        json!({"step_index": 5, "source": "MODEL", "type": "NOTIFY_USER", "status": "DONE", "input_tokens": 0, "content": {"notification": "  通知文字 "}}),
    ]
    .iter()
    .map(|v| format!("{v}\n"))
    .collect::<String>()
        + "{\"step_index\": 6, \"sour"
}

#[test]
fn macos_local_a_stop_is_spooled_with_its_event_name_and_the_reply_read_from_the_remote_transcript() {
    if !has_python3() {
        return;
    }
    let sb = Sandbox::new();
    sb.real_hook();
    let tp = sb.home.join(".gemini/antigravity-cli/brain/c-1/.system_generated/logs/transcript_full.jsonl");
    std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
    std::fs::write(&tp, transcript()).unwrap();
    let payload = json!({"conversationId": "c-1", "transcriptPath": tp, "terminationReason": "model_stop", "fullyIdle": true}).to_string();

    let (stdout, ok) = sb.run_in_pane("Stop", &payload, SYS_PATH);
    assert!(ok);
    assert_eq!(stdout, "{}\n", "hook 事件永遠只印 {{}}：stdout 會被 agy 當成決策");
    let events = sb.spooled();
    assert_eq!(events.len(), 1, "{events:?}");
    let e = &events[0];
    assert_eq!((e["provider"].as_str(), e["bot_id"].as_str()), (Some("agy"), Some("b-test")));
    let p = &e["payload"];
    assert_eq!(p["hookEventName"], "Stop");
    assert_eq!(p["conversationId"], "c-1", "原本的欄位不動");
    // 與本機 hook 子行程（`hook_cmd::enrich_agy_payload`）同一份語意：兩邊吃同一份 transcript，答案要一樣。
    let tail = transcript();
    let ex = cfg::last_exchange(&tail);
    assert_eq!(p["lastAssistantMessage"].as_str(), ex.assistant.as_deref());
    assert_eq!(p["lastUserMessage"].as_str(), ex.user.as_deref());
    assert_eq!(p["lastInputTokens"].as_i64(), cfg::last_input_tokens(&tail));
    assert_eq!(p["lastAssistantMessage"], "通知文字", "最後一段給使用者的文字（NOTIFY_USER 也算）");
    assert_eq!(p["lastUserMessage"], "現在部 \"demo\"\n第二行");
    assert_eq!(p["lastInputTokens"], 11824, "NOTIFY_USER 的 0 不算");
}

#[test]
fn macos_local_other_events_only_get_their_event_name_and_the_state_event_prints_nothing() {
    if !has_python3() {
        return;
    }
    let sb = Sandbox::new();
    sb.real_hook();
    let (stdout, ok) = sb.run_in_pane("PreInvocation", r#"{"conversationId":"c-9","transcriptPath":"/nonexistent/t.jsonl"}"#, SYS_PATH);
    assert!(ok);
    assert_eq!(stdout, "{}\n");
    // statusLine：stdout 會被當成狀態列文字，所以什麼都不印。
    let (stdout, ok) = sb.run_in_pane("state", r#"{"agent_state":"idle","conversation_id":"c-9","transcript_path":"/x/t.jsonl"}"#, SYS_PATH);
    assert!(ok);
    assert_eq!(stdout, "", "statusLine 不印東西");
    let events = sb.spooled();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0]["payload"]["hookEventName"], "PreInvocation");
    assert_eq!(events[1]["payload"]["hookEventName"], "state");
    assert_eq!(events[1]["payload"]["conversation_id"], "c-9");
    assert!(events[0]["payload"].get("lastAssistantMessage").is_none(), "只有 Stop 讀 transcript");
}

#[test]
fn macos_local_outside_a_daemon_pane_or_without_a_hook_sh_it_does_nothing_and_still_answers_empty_json() {
    let sb = Sandbox::new();
    sb.fake_hook();
    // 使用者自己開的 agy：沒有 AM_BOT_ID／AM_HOOK_TOKEN。
    let (stdout, ok) = sb.run("Stop", "{}", &[], SYS_PATH);
    assert!(ok);
    assert_eq!(stdout, "{}\n");
    assert!(!sb.home.join("argv").exists(), "沒有 pane env 就不轉給 hook.sh");
    // 隔離實例的 pane 不歸正式實例的 dispatcher。
    let (stdout, ok) = sb.run("Stop", "{}", &[("AM_BOT_ID", "b-test"), ("AM_HOOK_TOKEN", "t"), ("AM_INSTANCE", "other")], SYS_PATH);
    assert!(ok && stdout == "{}\n");
    assert!(!sb.home.join("argv").exists());
    // 這顆 bot 的 hook.sh 還沒裝：吞掉、不報錯。
    std::fs::remove_file(sb.bot_dir().join("hook.sh")).unwrap();
    let (stdout, ok) = sb.run_in_pane("Stop", "{}", SYS_PATH);
    assert!(ok && stdout == "{}\n");
    // 隔離實例的 dispatcher 只收自己實例的 pane。
    let iso = dispatch_sh("x/instances/iso", Some("iso"));
    assert!(iso.contains("AM_INSTANCE") && iso.contains("'iso'") && iso.contains("$HOME/x/instances/iso/bots/$AM_BOT_ID/hook.sh"), "{iso}");
}

#[test]
fn macos_local_without_python3_the_dispatcher_still_adds_the_event_name() {
    // PATH 只放必要的系統指令，刻意沒有 python3。
    let bin = tt::scratch_dir("am-agy-nopy-bin");
    for cmd in ["head", "tr", "cat", "sh", "printf"] {
        for dir in ["/usr/bin", "/bin"] {
            let src = Path::new(dir).join(cmd);
            if src.exists() {
                std::os::unix::fs::symlink(&src, bin.join(cmd)).unwrap();
                break;
            }
        }
    }
    let sb = Sandbox::new();
    sb.fake_hook();
    let path = bin.to_string_lossy().into_owned();
    let (stdout, ok) = sb.run_in_pane("SessionStart", r#"{"conversationId":"c-1","modelName":"m"}"#, &path);
    assert!(ok, "{stdout}");
    let got: Value = serde_json::from_str(&std::fs::read_to_string(sb.home.join("stdin")).unwrap()).expect("合法 JSON");
    assert_eq!((got["hookEventName"].as_str(), got["conversationId"].as_str()), (Some("SessionStart"), Some("c-1")));
    assert_eq!(std::fs::read_to_string(sb.home.join("argv")).unwrap().trim(), "agy b-test -", "參數＝provider、bot、token 佔位");
    // 空物件。
    let (_, ok) = sb.run_in_pane("Stop", "{}", &path);
    assert!(ok);
    let got: Value = serde_json::from_str(&std::fs::read_to_string(sb.home.join("stdin")).unwrap()).unwrap();
    assert_eq!(got, json!({"hookEventName": "Stop"}));
    // 事件名只留英文字母：不讓 hooks.json 的參數變成注入點。
    let (_, ok) = sb.run_in_pane("Stop\"; id #", "{}", &path);
    assert!(ok);
    let got: Value = serde_json::from_str(&std::fs::read_to_string(sb.home.join("stdin")).unwrap()).unwrap();
    assert_eq!(got, json!({"hookEventName": "Stopid"}));
}

#[test]
fn the_dispatcher_text_has_no_single_quote_inside_its_python_and_marks_itself_ours() {
    assert!(!ENRICH_PY.contains('\''));
    let s = dispatch_sh(ROOT, None);
    assert!(s.starts_with("#!/bin/sh\n") && s.contains("python3 -c '"), "{s}");
    assert!(cfg::DISPATCH_SH == "agy-hook.sh", "statusLine 認我們的指令靠檔名");
}

// ── 安裝（ssh 換成「就地用 /bin/sh 跑」，HOME 指到假家目錄）────────────────────────────────────────

async fn remote_conn(env: &tt::Env, home: &Path) -> (std::sync::Arc<HostConn>, String) {
    let host = format!("agy-rem-{}", crate::db::ulid().to_ascii_lowercase());
    let conn = env
        .app
        .hosts
        .insert_remote_for_test(HostCfg { shared_session: false, name: host.clone(), ssh: "unused".into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "am-test".into(), remote_path: String::new() })
        .await;
    *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
    let fake_home = home.to_path_buf();
    crate::hosts::set_ssh_fake(&host, move |script| {
        let out = Command::new("/bin/sh").arg("-c").arg(script).env("HOME", &fake_home).output()?;
        if !out.status.success() {
            anyhow::bail!("script failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    });
    (conn, host)
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[tokio::test]
async fn a_remote_install_writes_the_dispatcher_and_merges_only_our_keys_into_the_users_agy_files() {
    let env = tt::env().await;
    let home = tt::scratch_dir("am-agy-remote-install");
    let hooks = home.join(".gemini/config/hooks.json");
    let settings = home.join(".gemini/antigravity-cli/settings.json");
    std::fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let other_hook = json!({"enabled": true, "Stop": [{"type": "command", "command": "/home/u/mine.sh", "timeout": 3}]});
    std::fs::write(&hooks, json!({"my-hook": other_hook, "note": "keep"}).to_string()).unwrap();
    std::fs::write(&settings, json!({"trustedWorkspaces": ["/work/a"], "toolPermission": "request-review"}).to_string()).unwrap();
    let (conn, _host) = remote_conn(&env, &home).await;

    assert!(install_remote(&conn, None).await.unwrap(), "第一次要寫");
    let dispatcher = home.join(ROOT).join(cfg::DISPATCH_SH);
    assert_eq!(std::fs::metadata(&dispatcher).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::read_to_string(&dispatcher).unwrap(), dispatch_sh(ROOT, None));
    let h = read_json(&hooks);
    assert_eq!(h["my-hook"], other_hook, "別的具名 hook 原樣");
    assert_eq!(h["note"], "keep");
    for event in cfg::HOOK_EVENTS {
        let cmd = h["agents-manager"][event][0]["command"].as_str().unwrap();
        assert_eq!(cmd, format!("{} {event}", sh_quote(&dispatcher.to_string_lossy())), "{event}");
    }
    let s = read_json(&settings);
    assert_eq!(s["trustedWorkspaces"], json!(["/work/a"]));
    assert_eq!(s["toolPermission"], "request-review");
    assert_eq!(s["statusLine"]["command"], format!("{} state", sh_quote(&dispatcher.to_string_lossy())));
    assert_eq!(s["statusLine"]["stack_with_default"], true);

    assert!(!install_remote(&conn, None).await.unwrap(), "已經對了：不重寫");
}

#[tokio::test]
async fn a_remote_install_creates_missing_files_keeps_a_users_own_status_line_and_never_overwrites_unreadable_json() {
    let env = tt::env().await;
    // 全新的家目錄：兩個檔、整個 ~/.gemini 都要建出來（0600）。
    let home = tt::scratch_dir("am-agy-remote-fresh");
    let (conn, _) = remote_conn(&env, &home).await;
    assert!(install_remote(&conn, None).await.unwrap());
    let hooks = home.join(".gemini/config/hooks.json");
    assert_eq!(std::fs::metadata(&hooks).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(read_json(&hooks)["agents-manager"]["Stop"].is_array());

    // 使用者自己設了 statusLine：不碰（hook 照裝）。
    let home = tt::scratch_dir("am-agy-remote-userstatus");
    let settings = home.join(".gemini/antigravity-cli/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let mine = json!({"statusLine": {"type": "command", "command": "/home/u/mine-status.sh"}});
    std::fs::write(&settings, mine.to_string()).unwrap();
    let (conn, _) = remote_conn(&env, &home).await;
    install_remote(&conn, None).await.unwrap();
    assert_eq!(read_json(&settings), mine, "使用者的 statusLine 一個字不改");
    assert!(home.join(".gemini/config/hooks.json").is_file());

    // 讀不懂的 hooks.json：回錯、檔案原樣（agy 自己也是這樣）。
    let home = tt::scratch_dir("am-agy-remote-badjson");
    let hooks = home.join(".gemini/config/hooks.json");
    std::fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    std::fs::write(&hooks, "{ not json").unwrap();
    let (conn, _) = remote_conn(&env, &home).await;
    let err = install_remote(&conn, None).await.unwrap_err();
    assert!(format!("{err:#}").contains("not valid JSON"), "{err:#}");
    assert_eq!(std::fs::read_to_string(&hooks).unwrap(), "{ not json");
}

#[tokio::test]
async fn an_isolated_instance_gets_its_own_dispatcher_and_hook_name() {
    let env = tt::env().await;
    let home = tt::scratch_dir("am-agy-remote-iso");
    let (conn, _) = remote_conn(&env, &home).await;
    install_remote(&conn, None).await.unwrap();
    install_remote(&conn, Some("iso")).await.unwrap();
    let h = read_json(&home.join(".gemini/config/hooks.json"));
    assert!(h["agents-manager"].is_object() && h["agents-manager-iso"].is_object(), "各實例一個具名 hook：{h}");
    let iso = home.join(ROOT).join("instances/iso").join(cfg::DISPATCH_SH);
    assert!(iso.is_file());
    assert!(h["agents-manager-iso"]["Stop"][0]["command"].as_str().unwrap().contains("instances/iso/agy-hook.sh"));
}

/// 遠端 agy bot 的啟動注入：以前一律 bail「local host only」；現在裝 hook、argv 只帶權限旗標（沒有 hook 旗標）。
#[tokio::test]
async fn a_remote_agy_bot_start_installs_the_hooks_and_no_longer_bails() {
    let env = tt::env().await;
    let home = tt::scratch_dir("am-agy-remote-start");
    let (_conn, host) = remote_conn(&env, &home).await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "agy-remote").await;
    sqlx::query("UPDATE bots SET kind='agy', auto_approve=1 WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    let bot = crate::db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
    let mut project = crate::db::project(&env.app.db, &env.project_id).await.unwrap().unwrap();
    project.host = host;
    let args = crate::lifecycle::injected_args(&env.app, &bot, &project, &json!({})).await.expect("遠端 agy 不再被擋");
    assert_eq!(args, vec!["--dangerously-skip-permissions"], "agy 沒有 hook 旗標，只有權限旗標");
    // hook.sh（事件的去處）與 dispatcher、兩個設定檔都在那台的家目錄。
    assert!(home.join(ROOT).join("bots").join(&bot.id).join("hook.sh").is_file());
    assert!(home.join(ROOT).join(cfg::DISPATCH_SH).is_file());
    assert!(read_json(&home.join(".gemini/config/hooks.json"))["agents-manager"]["Stop"].is_array());
    assert!(read_json(&home.join(".gemini/antigravity-cli/settings.json"))["statusLine"]["command"].as_str().unwrap().ends_with("agy-hook.sh' state"));
}

/// hooks 裝不成（那台的 hooks.json 壞了）不擋啟動，跟本機一樣：少的只是 hook 回報。
#[tokio::test]
async fn a_remote_agy_start_survives_a_broken_hooks_file() {
    let env = tt::env().await;
    let home = tt::scratch_dir("am-agy-remote-broken");
    let hooks = home.join(".gemini/config/hooks.json");
    std::fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    std::fs::write(&hooks, "garbage").unwrap();
    let (_conn, host) = remote_conn(&env, &home).await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "agy-remote2").await;
    sqlx::query("UPDATE bots SET kind='agy', auto_approve=0 WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    let bot = crate::db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
    let mut project = crate::db::project(&env.app.db, &env.project_id).await.unwrap().unwrap();
    project.host = host;
    let args = crate::lifecycle::injected_args(&env.app, &bot, &project, &json!({})).await.expect("照常啟動");
    assert!(args.is_empty());
    assert_eq!(std::fs::read_to_string(&hooks).unwrap(), "garbage", "讀不懂的檔不覆寫");
}

/// 主機沒裝 agy 時啟動（preflight）要講清楚去哪裡裝，不是只說「請先安裝」（SPEC §12a.12）。
#[test]
fn starting_on_a_host_without_agy_points_at_the_install_prompt() {
    let msg = crate::kind_probe::verdict("m4p", "agy", Some(String::new())).unwrap_err();
    assert!(msg.contains("主機 m4p") && msg.contains("尚未安裝 agy") && msg.contains("安裝 agy") && msg.contains("sha512"), "{msg}");
    assert!(!msg.contains("remote_path"), "agy 不是靠 PATH 補丁解的");
    // 其他 kind 的訊息不變；查不了＝放行。
    assert!(crate::kind_probe::verdict("m4p", "codex", Some(String::new())).unwrap_err().contains("請先在該主機安裝 codex"));
    assert!(crate::kind_probe::verdict("m4p", "agy", None).is_ok());
    assert!(crate::kind_probe::verdict("m4p", "agy", Some("/Users/m4p/.local/bin/agy".into())).is_ok());
    // 本機。
    assert!(crate::kind_probe::verdict(crate::config::LOCAL_HOST, "agy", Some(String::new())).unwrap_err().starts_with("本機尚未安裝 agy"));
}
