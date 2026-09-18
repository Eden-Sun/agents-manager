//! 任務流程的轉移模型（issue #74 的 MissionController）：從**持久化的事實**推導任務現在在哪一關、
//! 下一步該做什麼、哪些關卡開著。
//!
//! 這裡全是純函式。輸入只有三樣：任務列、它的交辦（寫入順序）、它的事件串（寫入順序）；沒有任何記憶體
//! 裡的狀態，所以 daemon 重啟之後推得出同一個答案——「重啟後從持久狀態接續」靠的就是這一點
//! （接續的叫醒在 `workflow::wake_stalled`）。
//!
//! ## 分工
//! - **交辦的狀態**（`supervisor_assignments.status`）歸 AssignmentController（#71，`supervisor::assignment_state`）。
//!   這裡只**讀**（開著沒、`completed`／`failed`／`cancelled`／`superseded`），從不寫；要動交辦一律走
//!   supervisor 的入口（assign／review／followup）。
//! - **任務層**的轉移歸這裡：第幾代（generation）、這一代走到哪一關（[`Stage`]）、驗證與交付綁在哪個 commit、
//!   結案對交付的要求（[`delivery_requirement`]）。
//! - **需要判斷的**留在 AGM：am-review 是 approve 還是 changes、am-verify 過了沒、沒有獨立 reviewer 要不要跳過、
//!   要不要問使用者。這裡只把每個判斷點上**合法的分支**列出來（[`Next::alternatives`]），不替它選。
//!
//! ## 代（generation）
//! 每一則 `round`（review 退回或驗證失敗）開啟新的一代。事件屬於第幾代＝事件串裡排在它前面的 `round` 數；
//! 交辦屬於第幾代＝它**建立之前**已經有幾則 `round`。後者跨兩張表，時間戳只到毫秒、AGM 背靠背呼叫時會撞在
//! 同一格，所以 `round`／`verified` 寫下時在 payload 記 [`ANCHOR`]（當時最後一件交辦的 id，沒有就是 null），
//! 位置就在交辦清單裡比，不比時間。這個欄位出現之前的舊事件退回用時間比。
//!
//! ## 一代之內
//! 執行者被接受 → reviewer 被接受（或 AGM 判斷沒有獨立 reviewer、直接派驗證者）→ 驗證者被接受並記下
//! `verified(commit)` → `delivered(同一個 commit)` → 可以結案。`verified` 只在**同一代、而且之後沒有再派
//! 執行者**時算數：退回（新的一代）或再派執行者（rebase、補改）都代表成果可能變了，要重驗。交付關卡另外
//! 比 commit（HEAD 必須就是驗過的那個），兩道一起，舊一代的驗證怎樣都放行不了新的成果。
//!
//! ## 交辦的失敗與重試
//! 交辦被裁示 `fail`／`cancel`（那顆 bot 沒把工作做完）＝**同一個角色再派一次**（`retry_of`），不換關；
//! 工作做完了但**成果**不行（am-review 回 changes、am-verify 沒過）＝AGM 接受那件交辦、再 `round` 退回，
//! 這一代結束，下一代從執行者重做（`rework`）。兩件事分開，是因為前者換一顆 bot 就好，後者要改碼。

use crate::mission::store::{Mission, MissionEvent};
use crate::supervisor::store::Assignment;
use serde::Serialize;
use serde_json::{json, Value};

/// `round`／`verified` 的 payload 裡記「寫下時最後一件交辦是哪一件」的欄位（`null` = 當時一件都沒有）。
pub const ANCHOR: &str = "after_assignment";

/// 這一代走到哪一關（不看有沒有交辦開著、任務有沒有暫停——那兩件在 [`Flow::next`] 裡疊上去）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// 這一代還沒有被接受的執行成果：派執行者（第一次、退回後重做、或上一件沒做完再派）。
    NeedsExecutor,
    /// 執行成果被接受了、還沒審：派 reviewer（AGM 判斷沒有獨立 reviewer 時可以直接派驗證者）。
    NeedsReview,
    /// 審過了（或 AGM 已經跳過審查、派過驗證者）：派驗證者。
    NeedsVerification,
    /// 驗證者的交辦被接受了，卻沒有記 `verified`：AGM 讀 am-verify 判斷——通過記 `verified`，沒過 `round`。
    NeedsVerdict,
    /// 這一代有驗過的 commit，還沒交付。
    NeedsDelivery,
    /// 驗過的那個 commit 已經交付：可以結案。
    Delivered,
}

/// 一則 `verified` 事件的內容。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verified {
    pub event_id: String,
    /// 舊版事件沒記 commit 時是 `None`（交付關卡回 `verified_without_sha`）。
    pub sha: Option<String>,
    /// 驗證者驗的那個工作樹（有給才有）。交付要從執行者的工作樹交，HEAD 必須就是 `sha`。
    pub worktree: Option<String>,
    pub generation: usize,
}

/// 最新一則 `verified` 為什麼不能再放行交付。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stale {
    /// 之後有 `round`：成果被退回過，那是上一代的驗證。
    Round,
    /// 之後又派了執行者（rebase、補改、重做）：成果可能變了。
    NewExecutor,
}

#[derive(Debug, Clone)]
pub struct Flow<'a> {
    /// 目前是第幾代（`round` 的次數）。
    pub generation: usize,
    pub stage: Stage,
    /// 最新一則 `verified`，與它不能用的原因（`None` = 還算數）。
    pub latest_verified: Option<(Verified, Option<Stale>)>,
    /// 這一代驗過的 commit 已經交付：那一則 `delivered`。
    pub delivered: Option<&'a MissionEvent>,
    /// 還開著的那一件（同時只會有一件，見 `workflow::ensure_can_assign`；舊資料有兩件時取最後一件）。
    pub open: Option<&'a Assignment>,
    /// 這一代最後一件結案的交辦是被 `fail`／`cancel` 的：同一個角色要再派。
    pub retry_of: Option<&'a Assignment>,
    /// 這一代最後一件被接受的交辦（剛審完／剛驗完才有「退回」這條分支）。
    pub last_accepted: Option<&'a Assignment>,
    /// 這一代派過執行者沒有（任何狀態）。退回之後還沒派＝重做（`rework`）。
    pub executor_in_generation: bool,
}

fn payload(e: &MissionEvent) -> Value {
    serde_json::from_str(&e.payload_json).unwrap_or(Value::Null)
}

fn role(a: &Assignment) -> &str {
    a.mission_role.as_deref().unwrap_or("executor")
}

/// 事件寫下時已經存在幾件交辦（`assignments[..n]` 都在它之前）。
///
/// 有 [`ANCHOR`] 就照它在清單裡的位置；沒有（這個欄位之前的舊事件）或指到清單裡沒有的交辦，退回用時間比：
/// 建立時間不晚於事件的都算在前面。`assignments` 必須是 `mission_assignments` 的順序（建立時間、寫入順序）。
pub fn assignments_before(e: &MissionEvent, assignments: &[Assignment]) -> usize {
    let by_time = || assignments.iter().take_while(|a| a.created_at <= e.created_at).count();
    match payload(e).get(ANCHOR) {
        Some(Value::Null) => 0,
        Some(Value::String(id)) => assignments.iter().position(|a| &a.id == id).map_or_else(by_time, |i| i + 1),
        _ => by_time(),
    }
}

/// 一則 `delivered` 交的是哪個 commit（舊事件沒記就是 `None`）。
pub fn delivered_sha(e: &MissionEvent) -> Option<String> {
    payload(e).get("sha")?.as_str().map(str::to_string)
}

/// 從任務的交辦與事件推導流程。兩個清單都要是寫入順序（`mission_assignments`／`store::events` 給的就是）。
pub fn derive<'a>(assignments: &'a [Assignment], events: &'a [MissionEvent]) -> Flow<'a> {
    // 每一則 round：在事件串裡的位置、寫下時已經有幾件交辦。
    let rounds: Vec<(usize, usize)> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == "round")
        .map(|(i, e)| (i, assignments_before(e, assignments)))
        .collect();
    let generation = rounds.len();
    // 這一代從哪一件交辦開始。取最大值：錨點只會往後長，舊資料的時間比較就算有一點亂也不會把舊的一代算進來。
    let gen_start = rounds.iter().map(|r| r.1).max().unwrap_or(0).min(assignments.len());

    let mut executed = false;
    let mut verifier_done = false;
    let mut past_review = false;
    let mut last_executor: Option<usize> = None;
    let mut retry_of = None;
    let mut last_accepted = None;
    for (i, a) in assignments.iter().enumerate().skip(gen_start) {
        match role(a) {
            "executor" => last_executor = Some(i),
            // 派過驗證者＝審查這一關 AGM 已經放行（審過，或判斷沒有獨立 reviewer 而跳過）。
            "verifier" => past_review = true,
            _ => {}
        }
        match a.status.as_str() {
            "completed" => {
                retry_of = None;
                last_accepted = Some(a);
                match role(a) {
                    // 新的執行成果：之前的驗證者不算數（審查不重來，見模組說明）。
                    "executor" => {
                        executed = true;
                        verifier_done = false;
                    }
                    "reviewer" => past_review = true,
                    "verifier" => verifier_done = true,
                    _ => {}
                }
            }
            "failed" | "cancelled" => retry_of = Some(a),
            // superseded：它的 followup 接著做，本身不算結果。開著的另外看。
            _ => {}
        }
    }

    let latest_verified = events.iter().enumerate().rev().find(|(_, e)| e.kind == "verified").map(|(i, e)| {
        let p = payload(e);
        let v = Verified {
            event_id: e.id.clone(),
            sha: p.get("sha").and_then(Value::as_str).map(str::to_string),
            worktree: p.get("worktree").and_then(Value::as_str).map(str::to_string),
            generation: rounds.iter().filter(|r| r.0 < i).count(),
        };
        let stale = if v.generation < generation {
            Some(Stale::Round)
        } else if last_executor.is_some_and(|x| x >= assignments_before(e, assignments)) {
            Some(Stale::NewExecutor)
        } else {
            None
        };
        (v, stale)
    });
    let fresh_sha = match &latest_verified {
        Some((v, None)) => v.sha.clone(),
        _ => None,
    };
    let delivered = fresh_sha
        .as_deref()
        .and_then(|sha| events.iter().rev().find(|e| e.kind == "delivered" && delivered_sha(e).as_deref() == Some(sha)));

    let stage = match (&fresh_sha, delivered) {
        (Some(_), Some(_)) => Stage::Delivered,
        (Some(_), None) => Stage::NeedsDelivery,
        (None, _) if executed && verifier_done => Stage::NeedsVerdict,
        (None, _) if executed && past_review => Stage::NeedsVerification,
        (None, _) if executed => Stage::NeedsReview,
        (None, _) => Stage::NeedsExecutor,
    };
    Flow {
        generation,
        stage,
        latest_verified,
        delivered,
        open: assignments.iter().rev().find(|a| a.is_open()),
        retry_of,
        last_accepted,
        executor_in_generation: last_executor.is_some(),
    }
}

/// 下一步。`action` 是機器讀的；`hint` 是給 AGM 的一句話（怎麼做，不是要不要做）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Next {
    /// `assign` | `review` | `wait` | `record_verification` | `deliver` | `complete` | `paused` | `closed`
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// `review`／`wait`／`record_verification`：是哪一件交辦。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<String>,
    /// `assign`：上一件同角色的交辦沒做完（`fail`／`cancel`），這是重派。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_of: Option<String>,
    /// `assign executor`：被退回之後的重做（新開一件交辦，文字帶 findings）。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rework: bool,
    /// `deliver`：要交付的 commit（工作樹的 HEAD 必須就是它）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// `deliver`：驗證者記下的工作樹（有記才有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// 同一個判斷點上也合法的分支：`round`（成果被退回）、`skip_reviewer`（沒有獨立 reviewer）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<&'static str>,
    /// 任務暫停中。`action = paused` 時 `then` 是放行之後的那一步；`wait`／`review` 照常進行，只多這一欄。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub then: Option<Box<Next>>,
    pub hint: String,
}

impl Next {
    fn new(action: &'static str, hint: impl Into<String>) -> Self {
        Next {
            action,
            role: None,
            assignment_id: None,
            retry_of: None,
            rework: false,
            sha: None,
            worktree: None,
            alternatives: Vec::new(),
            paused_reason: None,
            then: None,
            hint: hint.into(),
        }
    }

    fn assign(role: &str, hint: impl Into<String>) -> Self {
        Next { role: Some(role.into()), ..Next::new("assign", hint) }
    }

    /// 輪到 AGM 動手、而且沒有任何東西會自己推進的那幾步（`wake_stalled` 只為這些叫醒）。
    pub fn is_agm_turn(&self) -> bool {
        matches!(self.action, "assign" | "record_verification" | "deliver" | "complete")
    }

    /// 這一步的穩定識別（同一步只叫醒一次）：動作、角色、第幾代、交辦數、commit。全部來自持久狀態。
    pub fn signature(&self, generation: usize, assignments: usize) -> String {
        format!(
            "{}:{}:g{generation}:a{assignments}:{}",
            self.action,
            self.role.as_deref().unwrap_or("-"),
            self.sha.as_deref().map(|s| &s[..12.min(s.len())]).unwrap_or("-")
        )
    }
}

fn short(sha: &str) -> &str {
    &sha[..12.min(sha.len())]
}

impl Flow<'_> {
    /// 還算數的驗證（同一代、之後沒有再派執行者、有記 commit）。
    pub fn fresh_verified(&self) -> Option<&Verified> {
        match &self.latest_verified {
            Some((v, None)) if v.sha.is_some() => Some(v),
            _ => None,
        }
    }

    /// `assign --mission` 現在可以派哪些角色。
    ///
    /// 只擋一件事：這一代還沒有被接受的執行成果時，不派 reviewer／驗證者——審一份不存在的成果、或退回之後
    /// 執行者還沒重做就重審舊的那份，都是流程跳了一步。其餘都放行：跳過 reviewer 是 AGM 的判斷，重派執行者
    /// （rebase、補改）在任何一關都合法。
    pub fn allowed_roles(&self) -> &'static [&'static str] {
        if self.stage == Stage::NeedsExecutor {
            &["executor"]
        } else {
            &["executor", "reviewer", "verifier"]
        }
    }

    /// 任務本身的狀態疊上去之前的那一步。
    pub fn step(&self) -> Next {
        if let Some(a) = self.open {
            let role = role(a).to_string();
            return if matches!(a.status.as_str(), "awaiting_review" | "blocked") {
                Next {
                    role: Some(role),
                    assignment_id: Some(a.id.clone()),
                    ..Next::new(
                        "review",
                        format!(
                            "交辦 {} 在等你裁示（{}）：`review` accept／followup／fail／cancel。裁示後任務的下一步 daemon 會重算",
                            a.id, a.status
                        ),
                    )
                }
            } else {
                Next {
                    role: Some(role),
                    assignment_id: Some(a.id.clone()),
                    ..Next::new(
                        "wait",
                        if a.status == "quota_blocked" {
                            "交辦在等額度，額度回來 daemon 自己重送；不用做任何事"
                        } else {
                            "交辦還在跑；回合結束會收到 assignment_completed"
                        },
                    )
                }
            };
        }
        let retry = self.retry_of.filter(|r| role(r) == self.role_for_stage().unwrap_or(""));
        let mut next = match self.stage {
            Stage::NeedsExecutor => {
                let rework = self.generation > 0 && !self.executor_in_generation;
                let hint = if rework {
                    format!("第 {} 輪退回：`mission pick --role executor` → 新開一件 `assign --mission <id> --role executor`，文字帶 findings（原本那件已結案，不能 followup）", self.generation)
                } else {
                    "`mission pick --role executor` → 開臨時 bot（`agm-mission-<id 尾 6 碼>-exec`）→ `assign --mission <id> --role executor`".to_string()
                };
                Next { rework, ..Next::assign("executor", hint) }
            }
            Stage::NeedsReview => Next {
                alternatives: vec!["skip_reviewer"],
                ..Next::assign(
                    "reviewer",
                    "`mission pick --role reviewer --exclude <執行者身分>` → `assign --mission <id> --role reviewer`；回 no_independent_reviewer 就記 note、直接派驗證者",
                )
            },
            Stage::NeedsVerification => Next {
                // 剛審完：am-review 回 changes 就是退回，不是往下走。
                alternatives: if self.last_accepted.is_some_and(|a| role(a) == "reviewer") { vec!["round"] } else { Vec::new() },
                ..Next::assign(
                    "verifier",
                    "`mission pick --role verifier` → `assign --mission <id> --role verifier`（am-review 回 changes 的話改走 `mission round` 退回執行者）",
                )
            },
            Stage::NeedsVerdict => Next {
                assignment_id: self.last_accepted.map(|a| a.id.clone()),
                alternatives: vec!["round"],
                ..Next::new(
                    "record_verification",
                    "讀驗證者的 am-verify：通過 → `mission event --kind verified --worktree <驗過的工作樹>`；沒過 → `mission round` 退回執行者",
                )
            },
            Stage::NeedsDelivery => {
                let v = self.fresh_verified();
                let sha = v.and_then(|v| v.sha.clone());
                Next {
                    hint: format!(
                        "`mission deliver <id> --worktree <執行者的工作樹>`：HEAD 必須是驗過的 {}",
                        sha.as_deref().map(short).unwrap_or("?")
                    ),
                    sha,
                    worktree: v.and_then(|v| v.worktree.clone()),
                    ..Next::new("deliver", "")
                }
            }
            Stage::Delivered => {
                let sha = self.fresh_verified().and_then(|v| v.sha.clone());
                Next {
                    hint: format!("{} 已交付：`mission complete <id> --text <結果摘要>`", sha.as_deref().map(short).unwrap_or("?")),
                    sha,
                    ..Next::new("complete", "")
                }
            }
        };
        if let Some(r) = retry {
            next.retry_of = Some(r.id.clone());
            next.hint = format!("上一件 {} 沒做完（{}），同一個角色再派一次：{}", r.id, r.status, next.hint);
        }
        next
    }

    fn role_for_stage(&self) -> Option<&'static str> {
        match self.stage {
            Stage::NeedsExecutor => Some("executor"),
            Stage::NeedsReview => Some("reviewer"),
            Stage::NeedsVerification => Some("verifier"),
            _ => None,
        }
    }

    /// 疊上任務本身的狀態：結案了就沒有下一步；暫停中，已經在跑／在等裁示的照常，其餘等放行。
    pub fn next(&self, m: &Mission) -> Next {
        if let s @ ("done" | "cancelled") = m.status() {
            return Next::new("closed", format!("任務已{}", if s == "done" { "完成" } else { "取消" }));
        }
        let step = self.step();
        match m.paused_reason.as_deref() {
            None => step,
            Some(reason) if matches!(step.action, "wait" | "review") => Next { paused_reason: Some(reason.into()), ..step },
            Some(reason) => {
                let hint = if matches!(reason, "push_main_failed" | "pr_failed") {
                    // 交付失敗是唯一一種「停著也要 AGM 自己動手」的：not_fast_forward 照 then 重做（rebase → 重驗 → 交付），
                    // 交付成功會自動解除暫停；其他失敗才問人（§18.14 第 5、10 步）。
                    format!("交付失敗（{reason}）：not_fast_forward 就照 `then` 派執行者 rebase、重驗、再交付（成功會自動解除暫停）；其他原因在群組問使用者")
                } else {
                    format!("任務停在 {reason}：在群組問使用者一個具體問題，放行（answer／resume）之後照 `then` 接續")
                };
                Next { paused_reason: Some(reason.into()), then: Some(Box::new(step)), ..Next::new("paused", hint) }
            }
        }
    }

    /// `mission get` 與裁示回應附的摘要。
    pub fn summary(&self) -> Value {
        json!({
            "generation": self.generation,
            "stage": self.stage,
            "allowed_roles": self.allowed_roles(),
            "verified": self.latest_verified.as_ref().map(|(v, stale)| json!({
                "event_id": v.event_id, "sha": v.sha, "generation": v.generation, "stale": stale,
            })),
            "delivered": self.delivered.map(|e| json!({"event_id": e.id, "sha": delivered_sha(e)})),
        })
    }
}

/// 結案時的「不交付」聲明（`POST /api/missions/{id}/complete` 的 `no_delivery`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiver {
    /// 這個任務沒有要交付的東西（只是查問題、寫報告）。
    NoChanges,
    /// 使用者決定不交付（交付失敗停下來問、或使用者改變主意）。
    UserDeclined,
}

impl Waiver {
    pub const ALL: [&'static str; 2] = ["no_changes", "user_declined"];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "no_changes" => Some(Waiver::NoChanges),
            "user_declined" => Some(Waiver::UserDeclined),
            _ => None,
        }
    }
}

/// 結案對交付的要求，**可機器判定**（issue #74）。回 `Ok(記錄)` 寫進 `completed` 事件；`Err((機器碼, 細節))` 是 409。
///
/// 1. 這一代驗過的 commit 已經交付 → 放行，記下交的是哪一個。有交付就不看聲明。
/// 2. 沒交付就必須明講為什麼（`no_delivery`），而且講的理由要對得上事實：
///    - `no_changes`：任務從來沒有 `verified`（驗過的 commit 就是要交付的東西）。有派過執行者的話，還要附上
///      執行者的工作樹，由呼叫端證明它乾淨、HEAD 已經在 base 裡（[`needs_worktree_proof`]）；
///    - `user_declined`：最近一次暫停之後，使用者本人（`relay_from` 空）回答過——問過人才說得出「使用者不要」。
///
/// 以前 `complete` 除了「還開著」什麼都不查：驗過卻沒交付、或根本沒驗就結案，成果卡上寫著「完成」，main 上什麼都沒有。
pub fn delivery_requirement(flow: &Flow, events: &[MissionEvent], waiver: Option<Waiver>) -> Result<Value, (&'static str, Value)> {
    if let Some(d) = flow.delivered {
        let p = payload(d);
        return Ok(json!({"status": "delivered", "sha": delivered_sha(d), "mode": p.get("mode"), "event_id": d.id}));
    }
    match waiver {
        None => Err((
            "not_delivered",
            json!({
                "stage": flow.stage,
                "verified_sha": flow.fresh_verified().and_then(|v| v.sha.clone()),
                "hint": "還沒交付：先 `mission deliver`；真的不需要交付，用 `--no-delivery no_changes`（沒有改東西）或 `user_declined`（使用者決定不交付）明講",
            }),
        )),
        Some(Waiver::NoChanges) => match events.iter().rev().find(|e| e.kind == "verified") {
            Some(v) => Err((
                "has_verified_changes",
                json!({
                    "verified_event_id": v.id,
                    "verified_sha": payload(v).get("sha").cloned(),
                    "hint": "這個任務驗過 commit，就是有東西要交：`mission deliver` 它，或使用者決定不交付時用 `user_declined`",
                }),
            )),
            None => Ok(json!({"status": "waived", "reason": "no_changes"})),
        },
        Some(Waiver::UserDeclined) => {
            let last_pause = events.iter().rposition(|e| e.kind == "paused");
            let answer = last_pause.and_then(|p| events[p + 1..].iter().rev().find(|e| e.kind == "answer" && e.relay_from.is_none()));
            match answer {
                Some(a) => Ok(json!({"status": "waived", "reason": "user_declined", "answer_event_id": a.id})),
                None => Err((
                    "user_not_asked",
                    json!({
                        "last_paused_event_id": last_pause.map(|p| events[p].id.clone()),
                        "hint": "沒有使用者的回答可以證明「使用者不要交付」：先 `mission pause --reason clarify` 在群組問，使用者回答之後再結案",
                    }),
                )),
            }
        }
    }
}

/// `no_changes` 要不要附執行者的工作樹來證明：派過執行者就要（它可能改了東西），從來沒派過就不用。
pub fn needs_worktree_proof(assignments: &[Assignment]) -> bool {
    assignments.iter().any(|a| role(a) == "executor")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 寫入順序的時間戳：第 n 筆就是第 n 毫秒。測試裡刻意**全部擠在同一毫秒**的版本另外測。
    fn ts(n: usize) -> String {
        format!("2026-09-18T00:00:{:02}.{:03}Z", n / 1000, n % 1000)
    }

    fn asg(id: &str, role: &str, status: &str, at: usize) -> Assignment {
        Assignment {
            id: id.into(),
            supervisor_id: "s".into(),
            request_id: None,
            target_bot_id: format!("bot-{id}"),
            client_request_id: format!("crid-{id}"),
            turn_id: None,
            text: "做事".into(),
            status: status.into(),
            delivery: None,
            result: None,
            error: None,
            attempts: 0,
            next_attempt_at: None,
            created_at: ts(at),
            updated_at: ts(at),
            completed_at: None,
            turn_status: None,
            evidence_complete: None,
            reviewed_at: None,
            reviewed_by: None,
            review_decision: None,
            review_reason: None,
            followup_assignment_id: None,
            follow_up_of: None,
            legacy_closed: 0,
            ownership_json: None,
            expects_review: 1,
            resume_at: None,
            quota_retries: 0,
            mission_id: Some("m".into()),
            mission_role: Some(role.into()),
            turn_error: None,
            review_role: None,
            conflict_since: None,
        }
    }

    fn ev(id: &str, kind: &str, payload: Value, at: usize) -> MissionEvent {
        MissionEvent {
            id: id.into(),
            mission_id: "m".into(),
            kind: kind.into(),
            text: kind.into(),
            relay_from: Some("daemon".into()),
            payload_json: payload.to_string(),
            reply_to: None,
            client_request_id: None,
            request_fingerprint: None,
            created_at: ts(at),
        }
    }

    fn user(mut e: MissionEvent) -> MissionEvent {
        e.relay_from = None;
        e
    }

    fn mission(paused: Option<&str>) -> Mission {
        Mission {
            id: "m".into(),
            project_id: "p".into(),
            client_request_id: "c".into(),
            text: "做一件事".into(),
            delivery_mode: "push_main".into(),
            executor_kind: "claude".into(),
            on_5h_limit: "wait".into(),
            max_rounds: 2,
            rounds_used: 0,
            paused_reason: paused.map(String::from),
            paused_detail: None,
            result_summary: None,
            parent_mission_id: None,
            request_fingerprint: None,
            created_at: ts(0),
            updated_at: ts(0),
            completed_at: None,
            cancelled_at: None,
        }
    }

    fn verified(id: &str, sha: &str, after: Option<&str>, at: usize) -> MissionEvent {
        ev(id, "verified", json!({"sha": sha, ANCHOR: after}), at)
    }

    fn round(id: &str, after: Option<&str>, at: usize) -> MissionEvent {
        ev(id, "round", json!({ANCHOR: after}), at)
    }

    fn delivered(id: &str, sha: &str, at: usize) -> MissionEvent {
        ev(id, "delivered", json!({"sha": sha, "mode": "push_main"}), at)
    }

    fn next_of(a: &[Assignment], e: &[MissionEvent]) -> Next {
        derive(a, e).next(&mission(None))
    }

    /// issue #74 驗收一：happy path 每一步的「下一步」都由 daemon 推出來，AGM 不用記 runbook。
    #[test]
    fn the_happy_path_derives_every_next_step() {
        let sha = "a".repeat(40);
        // 還沒有任何交辦：派執行者。
        let n = next_of(&[], &[]);
        assert_eq!((n.action, n.role.as_deref()), ("assign", Some("executor")));
        assert!(!n.rework, "第一次不是重做");

        // 執行者在跑：等；跑完等裁示：review 那一件。
        let n = next_of(&[asg("e1", "executor", "delivered", 1)], &[]);
        assert_eq!((n.action, n.assignment_id.as_deref()), ("wait", Some("e1")));
        let n = next_of(&[asg("e1", "executor", "awaiting_review", 1)], &[]);
        assert_eq!((n.action, n.assignment_id.as_deref()), ("review", Some("e1")));

        // 執行者被接受：派 reviewer（沒有獨立 reviewer 可以跳過——那是 AGM 的判斷，只列成分支）。
        let a = vec![asg("e1", "executor", "completed", 1)];
        let n = next_of(&a, &[]);
        assert_eq!((n.action, n.role.as_deref(), n.alternatives.clone()), ("assign", Some("reviewer"), vec!["skip_reviewer"]));

        // reviewer 被接受：派驗證者；am-review 回 changes 的話退回也合法。
        let a = vec![asg("e1", "executor", "completed", 1), asg("r1", "reviewer", "completed", 2)];
        let n = next_of(&a, &[]);
        assert_eq!((n.action, n.role.as_deref(), n.alternatives.clone()), ("assign", Some("verifier"), vec!["round"]));

        // 驗證者被接受、還沒記 verified：AGM 判讀 am-verify。
        let a = vec![asg("e1", "executor", "completed", 1), asg("r1", "reviewer", "completed", 2), asg("v1", "verifier", "completed", 3)];
        let n = next_of(&a, &[]);
        assert_eq!((n.action, n.assignment_id.as_deref(), n.alternatives.clone()), ("record_verification", Some("v1"), vec!["round"]));

        // 記了 verified：交付那個 commit。
        let e = vec![verified("ver", &sha, Some("v1"), 4)];
        let n = next_of(&a, &e);
        assert_eq!((n.action, n.sha.as_deref()), ("deliver", Some(sha.as_str())));

        // 交付了同一個 commit：結案。
        let e = vec![verified("ver", &sha, Some("v1"), 4), delivered("del", &sha, 5)];
        let f = derive(&a, &e);
        assert_eq!(f.stage, Stage::Delivered);
        assert_eq!(f.next(&mission(None)).action, "complete");
    }

    /// 跳過 reviewer（AGM 判斷沒有獨立 reviewer）之後，派過驗證者就算過了審查這一關：驗證者沒做完再派的也是驗證者，
    /// 不會又要求 reviewer。
    #[test]
    fn a_skipped_review_stays_skipped_when_the_verifier_is_retried() {
        let a = vec![asg("e1", "executor", "completed", 1), asg("v1", "verifier", "failed", 2)];
        let n = next_of(&a, &[]);
        assert_eq!((n.action, n.role.as_deref(), n.retry_of.as_deref()), ("assign", Some("verifier"), Some("v1")));
    }

    /// 驗收三：reviewer／驗證者／執行者的交辦沒做完（fail／cancel）＝同一個角色再派，不換關、不算一輪。
    #[test]
    fn a_failed_or_cancelled_assignment_is_retried_in_the_same_role() {
        for status in ["failed", "cancelled"] {
            let a = vec![asg("e1", "executor", status, 1)];
            let n = next_of(&a, &[]);
            assert_eq!((n.role.as_deref(), n.retry_of.as_deref()), (Some("executor"), Some("e1")), "{status}");

            let a = vec![asg("e1", "executor", "completed", 1), asg("r1", "reviewer", status, 2)];
            let n = next_of(&a, &[]);
            assert_eq!((n.role.as_deref(), n.retry_of.as_deref()), (Some("reviewer"), Some("r1")), "{status}");

            let a = vec![asg("e1", "executor", "completed", 1), asg("r1", "reviewer", "completed", 2), asg("v1", "verifier", status, 3)];
            let n = next_of(&a, &[]);
            assert_eq!((n.role.as_deref(), n.retry_of.as_deref()), (Some("verifier"), Some("v1")), "{status}");
            assert_eq!(derive(&a, &[]).generation, 0, "交辦沒做完不是退回，不開新的一代");
        }
        // 重派之後成功了就不再是重試。
        let a = vec![asg("e1", "executor", "failed", 1), asg("e2", "executor", "completed", 2)];
        let n = next_of(&a, &[]);
        assert_eq!((n.role.as_deref(), n.retry_of.as_deref()), (Some("reviewer"), None));
    }

    /// 驗收三＋八：成果被退回（round）＝新的一代，從執行者重做；reviewer／驗證者不能跳過重做直接重審舊的那份。
    #[test]
    fn a_round_starts_a_new_generation_that_begins_with_rework() {
        let a = vec![asg("e1", "executor", "completed", 1), asg("r1", "reviewer", "completed", 2)];
        let e = vec![round("rd1", Some("r1"), 3)];
        let f = derive(&a, &e);
        assert_eq!((f.generation, f.stage), (1, Stage::NeedsExecutor));
        assert_eq!(f.allowed_roles(), &["executor"], "退回之後執行者還沒重做，不能再審");
        let n = f.next(&mission(None));
        assert!(n.rework, "退回之後的執行者是重做");

        // 重做的執行者被接受：這一代要重新審。
        let a = vec![asg("e1", "executor", "completed", 1), asg("r1", "reviewer", "completed", 2), asg("e2", "executor", "completed", 4)];
        let n = next_of(&a, &e);
        assert_eq!((n.role.as_deref(), n.rework), (Some("reviewer"), false));
    }

    /// 驗收五～八：舊一代的驗證不能放行；驗完又派執行者（rebase、補改）也要重驗。
    #[test]
    fn a_verification_only_counts_for_its_own_generation_and_artifact() {
        let sha = "b".repeat(40);
        let a = vec![asg("e1", "executor", "completed", 1), asg("v1", "verifier", "completed", 2)];

        // 驗過之後退回：那是上一代的驗證。
        let e = vec![verified("ver", &sha, Some("v1"), 3), round("rd", Some("v1"), 4)];
        let f = derive(&a, &e);
        assert_eq!(f.latest_verified.as_ref().map(|(_, s)| *s), Some(Some(Stale::Round)));
        assert!(f.fresh_verified().is_none());
        assert_eq!(f.stage, Stage::NeedsExecutor);

        // 驗過之後又派了執行者（not_fast_forward → rebase）：成果可能變了，回到驗證那一關（審查不重來）。
        let a2 = vec![asg("e1", "executor", "completed", 1), asg("v1", "verifier", "completed", 2), asg("e2", "executor", "completed", 4)];
        let e = vec![verified("ver", &sha, Some("v1"), 3)];
        let f = derive(&a2, &e);
        assert_eq!(f.latest_verified.as_ref().map(|(_, s)| *s), Some(Some(Stale::NewExecutor)));
        assert_eq!(f.stage, Stage::NeedsVerification, "rebase 之後重驗，不重審");

        // 只是在跑、還沒被接受的執行者也一樣：HEAD 隨時會變。
        let a3 = vec![asg("e1", "executor", "completed", 1), asg("v1", "verifier", "completed", 2), asg("e2", "executor", "delivered", 4)];
        assert!(derive(&a3, &e).fresh_verified().is_none());

        // 同一代、之後沒有執行者：算數。
        let f = derive(&a, &e);
        assert_eq!(f.fresh_verified().and_then(|v| v.sha.as_deref()), Some(sha.as_str()));
    }

    /// 上一代交付過的 commit，這一代重驗了同一個：它確實已經交付，不用再交一次。
    #[test]
    fn delivery_is_matched_by_commit_not_by_order() {
        let sha = "c".repeat(40);
        let a = vec![asg("e1", "executor", "completed", 1)];
        let e = vec![verified("v-old", &sha, Some("e1"), 2), delivered("d", &sha, 3), round("rd", Some("e1"), 4)];
        assert_eq!(derive(&a, &e).stage, Stage::NeedsExecutor, "退回之後舊的交付不代表這一代做完了");
        let a = vec![asg("e1", "executor", "completed", 1), asg("e2", "executor", "completed", 5)];
        let mut e = e;
        e.push(verified("v-new", &sha, Some("e2"), 6));
        assert_eq!(derive(&a, &e).stage, Stage::Delivered);
        // 交的是別的 commit 不算。
        let e = vec![verified("v", &sha, Some("e1"), 2), delivered("d", &"d".repeat(40), 3)];
        assert_eq!(derive(&[asg("e1", "executor", "completed", 1)], &e).stage, Stage::NeedsDelivery);
    }

    /// 錨點不比時間：AGM 背靠背呼叫，round 與下一件交辦擠在同一毫秒，照樣分得出誰先誰後。
    #[test]
    fn anchors_order_events_and_assignments_within_the_same_millisecond() {
        let a = vec![asg("e1", "executor", "completed", 7), asg("r1", "reviewer", "completed", 7), asg("e2", "executor", "completed", 7)];
        // round 在 r1 之後、e2 之前寫下，三者同一毫秒。
        let e = vec![round("rd", Some("r1"), 7)];
        let f = derive(&a, &e);
        assert_eq!((f.generation, f.stage), (1, Stage::NeedsReview), "e2 是退回後的重做，r1 是上一代的審查");
        // 沒有交辦時的 round：錨點是 null，所有交辦都在它之後。
        let e = [round("rd", None, 9)];
        assert_eq!(assignments_before(&e[0], &a), 0);
    }

    /// 錨點出現之前的舊事件：退回用時間比，建立時間不晚於事件的交辦算在它前面。
    #[test]
    fn legacy_events_without_an_anchor_fall_back_to_time() {
        let a = vec![asg("e1", "executor", "completed", 1), asg("r1", "reviewer", "completed", 2), asg("e2", "executor", "completed", 5)];
        let e = vec![ev("rd", "round", json!({"rounds_used": 1}), 3)];
        assert_eq!(assignments_before(&e[0], &a), 2);
        assert_eq!(derive(&a, &e).stage, Stage::NeedsReview);
        // 錨點指到清單裡沒有的交辦：也退回時間比，不當成 0。
        let e = [ev("rd", "round", json!({ANCHOR: "gone"}), 3)];
        assert_eq!(assignments_before(&e[0], &a), 2);
    }

    /// 暫停疊在推導上面：在跑的、在等裁示的照常；其餘等放行，放行後的那一步放在 `then`。結案就沒有下一步。
    #[test]
    fn a_pause_holds_the_agm_steps_but_not_the_work_already_running() {
        let a = vec![asg("e1", "executor", "completed", 1)];
        let n = derive(&a, &[]).next(&mission(Some("max_rounds")));
        assert_eq!((n.action, n.paused_reason.as_deref()), ("paused", Some("max_rounds")));
        assert_eq!(n.then.as_ref().map(|t| (t.action, t.role.clone())), Some(("assign", Some("reviewer".into()))));
        assert!(!n.is_agm_turn(), "停下來問人的時候不是 AGM 自己能推進的一步");

        let a = vec![asg("e1", "executor", "awaiting_review", 1)];
        let n = derive(&a, &[]).next(&mission(Some("user_pause")));
        assert_eq!((n.action, n.paused_reason.as_deref()), ("review", Some("user_pause")), "回合結束照常驗收");

        let mut done = mission(None);
        done.completed_at = Some(ts(9));
        assert_eq!(derive(&a, &[]).next(&done).action, "closed");
    }

    /// 驗收七：結案對交付的要求可以機器判定，判定結果（交了哪個 commit、或為什麼不交）會被記下來。
    #[test]
    fn completing_requires_a_delivery_or_a_waiver_that_matches_the_facts() {
        let sha = "e".repeat(40);
        let a = vec![asg("e1", "executor", "completed", 1)];

        // 驗過、交付了：放行並記下 commit；有交付就不看聲明。
        let e = vec![verified("v", &sha, Some("e1"), 2), delivered("d", &sha, 3)];
        let f = derive(&a, &e);
        for w in [None, Some(Waiver::NoChanges), Some(Waiver::UserDeclined)] {
            let rec = delivery_requirement(&f, &e, w).unwrap();
            assert_eq!((rec["status"].as_str(), rec["sha"].as_str()), (Some("delivered"), Some(sha.as_str())));
        }

        // 驗過沒交付、什麼都沒講：擋。
        let e = vec![verified("v", &sha, Some("e1"), 2)];
        let f = derive(&a, &e);
        assert_eq!(delivery_requirement(&f, &e, None).unwrap_err().0, "not_delivered");
        // 驗過的 commit 不能說成「沒有改東西」。
        assert_eq!(delivery_requirement(&f, &e, Some(Waiver::NoChanges)).unwrap_err().0, "has_verified_changes");
        // 沒問過使用者，不能說「使用者不要」。
        assert_eq!(delivery_requirement(&f, &e, Some(Waiver::UserDeclined)).unwrap_err().0, "user_not_asked");

        // 交付失敗停下來、使用者本人回答之後：可以用 user_declined 結案，記下是哪一則回答。
        let mut e2 = e.clone();
        e2.push(ev("p", "paused", json!({"reason": "push_main_failed"}), 3));
        e2.push(ev("bot-answer", "answer", json!({}), 4)); // AGM 回的不算
        assert_eq!(delivery_requirement(&f, &e2, Some(Waiver::UserDeclined)).unwrap_err().0, "user_not_asked");
        e2.push(user(ev("ans", "answer", json!({}), 5)));
        let rec = delivery_requirement(&f, &e2, Some(Waiver::UserDeclined)).unwrap();
        assert_eq!((rec["reason"].as_str(), rec["answer_event_id"].as_str()), (Some("user_declined"), Some("ans")));
        // 回答在**最近一次**暫停之前的不算：那是回答上一個問題。
        e2.push(ev("p2", "paused", json!({"reason": "clarify"}), 6));
        assert_eq!(delivery_requirement(&f, &e2, Some(Waiver::UserDeclined)).unwrap_err().0, "user_not_asked");

        // 從來沒驗過（只是查問題）：可以用 no_changes。
        let f = derive(&a, &[]);
        assert_eq!(delivery_requirement(&f, &[], Some(Waiver::NoChanges)).unwrap()["reason"], "no_changes");
        assert!(needs_worktree_proof(&a), "派過執行者就要附工作樹證明沒有改動");
        assert!(!needs_worktree_proof(&[]));
    }
}
