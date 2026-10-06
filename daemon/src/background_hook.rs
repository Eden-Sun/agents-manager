//! claude 的 Stop hook 自己報「背景還有什麼在跑」（claude ≥ 2.1.287，issue #714 的後續）。
//!
//! Stop payload 多了兩個欄位（2.1.287 執行檔裡的 zod schema，正式環境 `hook_events` 的真 payload 對得上）：
//! - `background_tasks[]`：這個 session 還在跑的背景工作（running／pending／backgrounded）。`{id, type, status, description}`，
//!   `type` 是 `shell`／`subagent`／`monitor`／`workflow`…，shell 另有 `command`，subagent 有 `agent_type`，monitor／MCP 有 `server`／`tool`，
//!   workflow 有 `name`。沒有東西在跑時是**空陣列**（不是沒有這個鍵）。
//! - `session_crons[]`：session 範圍的排程（CronCreate、ScheduleWakeup、/loop）：`{id, schedule, recurring, prompt}`，之後會把 session 叫醒。
//!   只當資訊顯示，**不算背景工作**（不擋一鍵重啟，行為跟畫面判斷時代一樣）。
//!
//! 有 `background_tasks` 的 Stop 以它為準（比畫面準、回合一結束就有，不用等 30 秒巡邏）；沒有這個鍵（舊版 claude、遠端舊 hook）
//! 完全不碰，維持 `background_jobs.rs` 的畫面判斷。畫面判斷也沒拿掉，它仍是巡邏的常規讀法，也用來校正過期的 hook 帳：
//! 見 [`GRACE`]。常駐服務（背景 shell 子樹在 listen TCP port）照舊不算（SPEC §6.14）。

use crate::db;
use crate::state::App;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// hook 帳在這段時間內不被畫面推翻（畫面巡邏 30 秒一輪，模式列也可能慢半拍）。超過之後 hook 帳退場，改採目前畫面。
pub const GRACE: Duration = Duration::from_secs(60);

/// 回給前端的清單上限與單欄長度（`/api/state` 每次都帶，不能因為一個 1000 字的命令就膨脹）。
const MAX_LISTED: usize = 20;
const CLIP: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub description: String,
    pub command: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    pub id: String,
    pub schedule: String,
    pub recurring: bool,
    pub prompt: String,
}

/// 一則 Stop 報的內容。`None`＝payload 沒有 `background_tasks`（舊版 claude）。
#[derive(Debug, Clone, PartialEq)]
pub struct Reported {
    pub tasks: Vec<Task>,
    pub crons: Vec<Cron>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub at: Instant,
    pub reported: Reported,
    /// 常駐服務的 shell 數（背景 shell 子樹在 listen port）；`None`＝還沒查到，先不扣（寧可多標）。
    pub services: Option<u32>,
}

/// `App.background_hook`：run id → 最近一則 Stop 報的內容。記憶體，跟 `background_jobs` 同壽命。
pub type Snapshots = Mutex<HashMap<String, Snapshot>>;

pub fn parse(payload: &Value) -> Option<Reported> {
    let list = payload.get("background_tasks")?.as_array()?;
    let text = |o: &Value, k: &str| o.get(k).and_then(Value::as_str).map(str::to_string);
    let tasks = list
        .iter()
        .filter(|t| t.is_object())
        .map(|t| Task {
            id: text(t, "id").unwrap_or_default(),
            kind: text(t, "type").filter(|k| !k.is_empty()).unwrap_or_else(|| "unknown".into()),
            status: text(t, "status").unwrap_or_default(),
            description: text(t, "description").unwrap_or_default(),
            command: text(t, "command"),
        })
        .collect();
    let crons = payload
        .get("session_crons")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|c| c.is_object())
                .map(|c| Cron {
                    id: text(c, "id").unwrap_or_default(),
                    schedule: text(c, "schedule").unwrap_or_default(),
                    recurring: c.get("recurring").and_then(Value::as_bool).unwrap_or(false),
                    prompt: text(c, "prompt").unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Reported { tasks, crons })
}

impl Reported {
    fn shells(&self) -> u32 {
        self.tasks.iter().filter(|t| t.kind == "shell").count() as u32
    }
}

impl Snapshot {
    /// 要顯示的背景工作數：shell 以外的全算，shell 扣掉常駐服務。
    pub fn jobs(&self) -> u32 {
        let shells = self.reported.shells();
        let others = self.reported.tasks.len() as u32 - shells;
        others + shells.saturating_sub(self.services.unwrap_or(0))
    }
}

fn clip(s: &str) -> String {
    if s.chars().count() <= CLIP {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(CLIP).collect::<String>())
    }
}

/// claude 的 Stop hook 進來：有 `background_tasks` 就記下並更新 `background_jobs` 的數字（變了推 `bot_status`）。
/// 沒有這個鍵（舊版 claude）什麼都不做，數字留給畫面判斷。
pub async fn on_stop(app: &Arc<App>, run: &db::Run, payload: &Value) {
    let Some(reported) = parse(payload) else { return };
    let snap = Snapshot { at: Instant::now(), reported, services: None };
    let (n, had_shells) = (snap.jobs(), snap.reported.shells() > 0);
    let at = snap.at;
    let changed = {
        let mut hook = app.background_hook.lock().unwrap_or_else(|e| e.into_inner());
        let mut counts = app.background_jobs.lock().unwrap_or_else(|e| e.into_inner());
        let counted = crate::background_jobs::record(&mut counts, &run.id, n);
        let before = hook.get(&run.id).map(|s| s.reported.clone());
        let after = Some(snap.reported.clone());
        hook.insert(run.id.clone(), snap);
        counted || before != after
    };
    if changed {
        tracing::info!(run = %run.id, bot = %run.bot_id, background_jobs = n, source = "hook", "background jobs changed");
        app.emit_bot_status(&run.bot_id).await;
    }
    if had_shells {
        // 常駐服務要看行程樹（本機 ps、遠端 ssh，最多 10 秒）：不卡住 hook 的處理，查到再回頭修正數字。
        let (app, run) = (app.clone(), run.clone());
        tokio::spawn(async move { refine_services(&app, &run, at).await });
    }
}

async fn refine_services(app: &(impl crate::background_hook::HookSnapshots + crate::background_jobs::JobCounts + crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), run: &db::Run, at: Instant) {
    let Some(pane) = run.pane_id.as_deref() else { return };
    let Some(client) = app.herdr_for_run(run).await else { return };
    let services = crate::background_jobs::services(app, run, &client, pane).await;
    let n = {
        let mut hook = app.background_hook().lock().unwrap_or_else(|e| e.into_inner());
        // 這段時間裡來了新的 Stop（或被丟掉）：這份查詢結果不屬於它了。
        let Some(snap) = hook.get_mut(&run.id).filter(|s| s.at == at) else { return };
        snap.services = Some(services);
        snap.jobs()
    };
    let changed = crate::background_jobs::record(&mut app.background_jobs().lock().unwrap_or_else(|e| e.into_inner()), &run.id, n);
    if changed {
        tracing::info!(run = %run.id, bot = %run.bot_id, background_jobs = n, services, "background jobs changed (resident services deducted)");
        app.emit_bot_status(&run.bot_id).await;
    }
}

/// 巡邏讀到畫面時的取捨：回傳要記的數字，必要時丟掉過期的 hook 帳。`screen` 是畫面判斷的結果（已扣常駐服務）。
pub fn reconcile(app: &impl crate::background_hook::HookSnapshots, run_id: &str, screen: u32) -> u32 {
    let mut hook = app.background_hook().lock().unwrap_or_else(|e| e.into_inner());
    let Some(snap) = hook.get(run_id) else { return screen };
    let jobs = snap.jobs();
    if snap.at.elapsed() < GRACE {
        return jobs;
    }
    hook.remove(run_id);
    screen
}

/// 有 hook 帳的 run 的 API 欄位（`background_tasks`／`session_crons`）；沒有就是 `null`。
pub fn details(app: &impl crate::background_hook::HookSnapshots, run_id: &str) -> (Value, Value) {
    let hook = app.background_hook().lock().unwrap_or_else(|e| e.into_inner());
    let Some(snap) = hook.get(run_id) else { return (Value::Null, Value::Null) };
    let tasks: Vec<Value> = snap
        .reported
        .tasks
        .iter()
        .take(MAX_LISTED)
        .map(|t| {
            let mut o = json!({"id": t.id, "type": t.kind, "status": t.status, "description": clip(&t.description)});
            if let Some(c) = &t.command {
                o["command"] = json!(clip(c));
            }
            o
        })
        .collect();
    let crons: Vec<Value> = snap
        .reported
        .crons
        .iter()
        .take(MAX_LISTED)
        .map(|c| json!({"id": c.id, "schedule": c.schedule, "recurring": c.recurring, "prompt": clip(&c.prompt)}))
        .collect();
    (json!(tasks), json!(crons))
}

pub fn retain_runs(app: &impl crate::background_hook::HookSnapshots, active: &[String]) {
    app.background_hook().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    fn fixture() -> Value {
        let path = format!("{}/src/lifecycle/fixtures/claude-2.1.287-stop-background-tasks.json", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn the_real_payload_shape_parses_and_an_absent_key_means_an_old_claude() {
        let r = parse(&fixture()).expect("2.1.287 payload");
        assert_eq!(r.tasks.len(), 2);
        assert_eq!((r.tasks[0].kind.as_str(), r.tasks[0].status.as_str()), ("shell", "running"));
        assert_eq!(r.tasks[0].command.as_deref(), Some("sleep 600 && echo done"));
        assert_eq!(r.crons, vec![Cron { id: "c1".into(), schedule: "30 14 2 10 *".into(), recurring: false, prompt: "check the build".into() }]);

        let mut old = fixture();
        old.as_object_mut().unwrap().remove("background_tasks");
        assert_eq!(parse(&old), None, "舊版 claude 沒有這個鍵：不是『零個』，是『沒說』");
        let mut empty = fixture();
        empty["background_tasks"] = json!([]);
        assert_eq!(parse(&empty).map(|r| r.tasks.len()), Some(0), "空陣列＝說了：沒有");
        let mut odd = fixture();
        odd["background_tasks"] = json!("nope");
        assert_eq!(parse(&odd), None, "形狀不對當沒說（退回畫面）");
        // 欄位缺的元素照收：type 不明就叫 unknown，仍然算一個在跑的工作。
        let mut sparse = fixture();
        sparse["background_tasks"] = json!([{"id": "x"}, 7]);
        assert_eq!(parse(&sparse).unwrap().tasks.len(), 1, "非物件的元素略過");
        assert_eq!(parse(&sparse).unwrap().tasks[0].kind, "unknown");
    }

    #[test]
    fn only_shells_can_be_services_and_everything_else_counts() {
        let mut p = fixture();
        p["background_tasks"] = json!([
            {"id": "1", "type": "shell", "status": "running", "description": "a"},
            {"id": "2", "type": "shell", "status": "running", "description": "dev server"},
            {"id": "3", "type": "subagent", "status": "running", "description": "review", "agent_type": "Explore"},
        ]);
        let mut s = Snapshot { at: Instant::now(), reported: parse(&p).unwrap(), services: None };
        assert_eq!(s.jobs(), 3, "服務還沒查到：不扣");
        s.services = Some(1);
        assert_eq!(s.jobs(), 2);
        s.services = Some(9);
        assert_eq!(s.jobs(), 1, "扣到只剩非 shell 為止，不會變負的");
    }

    async fn setup() -> (tt::Env, db::Run) {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        let run = db::run(&env.app.db, &run_id).await.unwrap().unwrap();
        (env, run)
    }

    /// Stop 一到就有數字、清單，並推 `bot_status`；下一則 Stop（空陣列）立刻歸零；沒有這個鍵的 Stop 什麼都不動。
    #[tokio::test]
    async fn a_stop_with_background_tasks_sets_the_count_and_the_details_at_once() {
        let (env, run) = setup().await;
        let app = env.app.clone();
        let mut rx = app.subscribe();
        assert_eq!(crate::background_jobs::known(&app, &run.id), None);

        on_stop(&app, &run, &fixture()).await;
        assert_eq!(crate::background_jobs::known(&app, &run.id), Some(2), "不用等 30 秒巡邏");
        let frame = rx.try_recv().expect("數字變了要推 bot_status");
        assert_eq!(frame.kind, "bot_status");
        let v = crate::background_jobs::run_json(&app, &Some(run.clone()), Some(&run.id));
        assert_eq!(v["background_jobs"], 2);
        assert_eq!(v["background_source"], "hook");
        assert_eq!(v["background_tasks"][0]["description"], "wait for the remote build");
        assert_eq!(v["background_tasks"][0]["type"], "shell");
        assert_eq!(v["session_crons"][0]["schedule"], "30 14 2 10 *");

        let mut none = fixture();
        none.as_object_mut().unwrap().remove("background_tasks");
        on_stop(&app, &run, &none).await;
        assert_eq!(crate::background_jobs::known(&app, &run.id), Some(2), "沒有這個鍵：不動，留給畫面判斷");

        let mut empty = fixture();
        empty["background_tasks"] = json!([]);
        on_stop(&app, &run, &empty).await;
        assert_eq!(crate::background_jobs::known(&app, &run.id), Some(0));
        let v = crate::background_jobs::run_json(&app, &Some(run.clone()), Some(&run.id));
        assert_eq!(v["background_tasks"], json!([]), "報過『沒有』也是證據：清單是空的、不是 null");
        assert_eq!(v["session_crons"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn the_screen_does_not_overrule_a_fresh_hook_report_but_does_overrule_a_stale_one() {
        let (env, run) = setup().await;
        let app = env.app.clone();
        on_stop(&app, &run, &fixture()).await;
        // 畫面（還沒更新）說 0：剛報的 hook 帳不被推翻。
        assert_eq!(reconcile(&app, &run.id, 0), 2);
        // 過了寬限、畫面仍然完全沒有背景：背景在沒有新 Stop 的情況下結束了，丟掉 hook 帳。
        app.background_hook.lock().unwrap().get_mut(&run.id).unwrap().at = Instant::now() - GRACE - Duration::from_secs(1);
        assert_eq!(reconcile(&app, &run.id, 0), 0);
        assert_eq!(details(&app, &run.id), (Value::Null, Value::Null), "丟掉就沒有清單了");
        // 沒有 hook 帳＝畫面的數字。
        assert_eq!(reconcile(&app, &run.id, 3), 3);

        // 過了寬限，畫面有一個 shell：以現場數字取代舊 hook 的兩個 shell。
        on_stop(&app, &run, &fixture()).await;
        app.background_hook.lock().unwrap().get_mut(&run.id).unwrap().at = Instant::now() - GRACE - Duration::from_secs(1);
        assert_eq!(reconcile(&app, &run.id, 1), 1);
        assert_eq!(details(&app, &run.id), (Value::Null, Value::Null), "stale shell-only details are dropped");
        // hook 說 0：寬限內畫面說 1 也是 0；過了寬限就退場、只看畫面。
        let mut empty = fixture();
        empty["background_tasks"] = json!([]);
        on_stop(&app, &run, &empty).await;
        assert_eq!(reconcile(&app, &run.id, 1), 0);
        app.background_hook.lock().unwrap().get_mut(&run.id).unwrap().at = Instant::now() - GRACE - Duration::from_secs(1);
        assert_eq!(reconcile(&app, &run.id, 1), 1);
        assert_eq!(details(&app, &run.id), (Value::Null, Value::Null));
    }

    /// Once the hook snapshot is stale, the pane screen owns the shell count. A finished shell must
    /// not remain counted just because another shell from the same Stop report is still visible.
    #[tokio::test]
    async fn a_stale_hook_shell_count_is_replaced_by_the_current_screen_count() {
        let (env, run) = setup().await;
        let app = env.app.clone();
        let payload = json!({
            "background_tasks": [
                {"id": "shell-1", "type": "shell", "status": "running", "description": "sleep 1"},
                {"id": "shell-2", "type": "shell", "status": "running", "description": "sleep 2"}
            ],
            "session_crons": []
        });
        on_stop(&app, &run, &payload).await;
        app.background_hook.lock().unwrap().get_mut(&run.id).unwrap().at = Instant::now() - GRACE - Duration::from_secs(1);

        assert_eq!(reconcile(&app, &run.id, 1), 1, "the stale hook said two, but only one shell remains on screen");
        assert_eq!(details(&app, &run.id), (Value::Null, Value::Null), "screen-derived counts must not expose stale hook task details");
    }

    /// A hook snapshot is authoritative only inside its freshness window. Once stale, even a
    /// subagent/workflow report must yield to the current screen fallback.
    #[tokio::test]
    async fn a_stale_non_shell_hook_task_falls_back_to_the_current_screen() {
        let (env, run) = setup().await;
        let app = env.app.clone();
        let payload = json!({
            "background_tasks": [{
                "id": "agent-1", "type": "subagent", "status": "running", "description": "review"
            }],
            "session_crons": []
        });
        on_stop(&app, &run, &payload).await;
        app.background_hook.lock().unwrap().get_mut(&run.id).unwrap().at = Instant::now() - GRACE - Duration::from_secs(1);

        assert_eq!(reconcile(&app, &run.id, 2), 2, "stale hook data yields to current screen count");
        assert_eq!(details(&app, &run.id), (Value::Null, Value::Null), "stale hook details are no longer authoritative");
    }

    #[tokio::test]
    async fn a_run_that_ended_leaves_nothing_behind() {
        let (env, run) = setup().await;
        on_stop(&env.app, &run, &fixture()).await;
        retain_runs(&env.app, &[]);
        assert_eq!(details(&env.app, &run.id), (Value::Null, Value::Null));
    }

    /// 端到端：真的走 `hookrecv::process`（fence、bot 鎖、classify），Stop 的 `background_tasks` 進到 API 的 run 物件；
    /// 舊世代（run_id 對不上）的 Stop 不算。
    #[tokio::test]
    async fn the_hook_pipeline_feeds_it_and_a_stale_generation_does_not() {
        let (env, run) = setup().await;
        let app = env.app.clone();
        let body = |run_id: Option<&str>| crate::hookrecv::HookBody {
            bot_id: run.bot_id.clone(),
            provider: "claude".into(),
            payload: fixture(),
            received_at: None,
            truncated: false,
            run_id: run_id.map(String::from),
        };
        crate::hookrecv::process(&app, &body(Some("some-older-run"))).await.unwrap();
        assert_eq!(crate::background_jobs::known(&app, &run.id), None, "上一代的 hook 不改這一代的帳");
        crate::hookrecv::process(&app, &body(Some(&run.id))).await.unwrap();
        assert_eq!(crate::background_jobs::known(&app, &run.id), Some(2));
        let state = crate::api::state_json(&app).await.unwrap();
        assert_eq!(state["projects"][0]["bots"][0]["run"]["background_tasks"][1]["description"], "dev server");
    }
}

/// 背景 hook 快照。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait HookSnapshots: Send + Sync {
    fn background_hook(&self) -> &crate::background_hook::Snapshots;
}
