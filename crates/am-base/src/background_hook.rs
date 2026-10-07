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

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
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
    pub fn shells(&self) -> u32 {
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



/// 背景 hook 快照。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait HookSnapshots: Send + Sync {
    fn background_hook(&self) -> &crate::background_hook::Snapshots;
}
