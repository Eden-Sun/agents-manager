//! 一鍵把「等著套用更新」的閒置 bot 全部 exit + resume（SPEC §6.9）。
//!
//! 跑的部分疊在既有單顆路徑（stop + `resume_native` start）上，沒有另一套啟動流程。
//! 挑的規則刻意保守：只動帶著 update_notice 的閒置 claude／codex——批次最不能做的就是砍掉使用者正在等的回合。
//!
//! **codex 也進批次，但只收「磁碟已裝好」的那一種**（2026-09-22，issue「codex 有更新怎沒出現在
//! header」）：codex 的更新通知有兩種文案（`codex_update.rs`）——磁碟已經裝好、這個 run 還跑舊版
//! （notice 含「已安裝」）跟 claude 完全一樣，重啟就換，可以進批次；新版**還沒安裝**（notice 含
//! 「需安裝」）重啟一顆沒裝新版的 codex 換不到任何東西，所以仍然不進批次自動重啟，但要留在候選名單
//! 裡、標成「需要手動安裝」才會出現在 header 與批次框（`Skip::NeedsManualInstall`），不能像以前那樣
//! 整顆連候選都不算、在 header 上完全消失。

/// 純資料，好寫測試。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cand {
    pub bot_id: String,
    pub name: String,
    pub kind: String,
    pub managed_by: String,
    /// 子 agent（`managed_by = child` 或 `parent_bot_id` 非空）：pane 是父 bot 用 herdr 開的，批次不動它（SPEC §6.5a）。
    pub child: bool,
    pub state: String,
    pub agent_status: String,
    pub has_update: bool,
    /// codex 專屬：notice 說的是「還沒裝，要先手動裝」而不是「已經裝好，重啟就換」。
    pub needs_manual_install: bool,
    pub turn_in_flight: bool,
    /// 使用者自己的 herdr `default` session（SPEC §6.5.1）：daemon 只觀察，不開、不關它的 pane。
    pub default_session: bool,
    /// #714／#767：畫面底部標著的背景工作數；`None`＝巡邏還沒看過這個 run（沒有證據，不擋）。
    pub background_jobs: Option<u32>,
}

/// `code` 給 API / 前端比對，`label` 給人看。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// 子 agent：由父 bot 用 herdr 重開，daemon 不動（2026-09-22 rollout 對 pvd／rh 下 restart，herdr 回 agent_name_taken，
    /// 兩顆被 reconcile 退役軟刪）。
    Child,
    DefaultSession,
    NotRunning,
    Working,
    Blocked,
    UnknownStatus,
    TurnInFlight,
    /// codex 新版還沒安裝，重啟一顆沒裝新版的 codex 換不到任何東西——要先手動跑 notice 裡的安裝指令。
    NeedsManualInstall,
    /// 排到它的時候已經不用重啟了：更新套用過、run 不在了、bot 被刪了。
    NoLongerPending,
    /// 輪到它時 DB 讀不到它的狀態（一時忙、I/O 錯）：不知道≠不用重啟，這次先不動，更新還在等（#188）。
    StateUnreadable,
    /// scoped restart 的原主機連線已被移除或改指，不能用相同名字改重啟新主機上的 bot。
    HostSuperseded,
    /// 回合結束、agent 閒置，但畫面底部還標著 N 個背景工作（#714）：重啟一退 CLI，背景 shell／終端跟著沒了（#767）。
    BackgroundJobs(u32),
}

impl Skip {
    pub fn code(self) -> &'static str {
        match self {
            Skip::Child => "child",
            Skip::DefaultSession => "default_session",
            Skip::NotRunning => "not_running",
            Skip::Working => "working",
            Skip::Blocked => "blocked",
            Skip::UnknownStatus => "unknown_status",
            Skip::TurnInFlight => "turn_in_flight",
            Skip::NeedsManualInstall => "needs_manual_install",
            Skip::NoLongerPending => "no_longer_pending",
            Skip::StateUnreadable => "state_unreadable",
            Skip::HostSuperseded => "superseded",
            Skip::BackgroundJobs(_) => "background_jobs",
        }
    }

    pub fn label(self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow::{Borrowed, Owned};
        if let Skip::BackgroundJobs(n) = self {
            return Owned(format!("背景執行中（{n}）"));
        }
        Borrowed(match self {
            Skip::Child => "子 agent：由父 bot 用 herdr 重開，daemon 不動它的 pane（SPEC §6.5a）",
            Skip::DefaultSession => "在你自己的 herdr default session 裡，daemon 不動它的 pane",
            Skip::NotRunning => "還在啟動或關閉中",
            Skip::Working => "正在跑，重啟會把這一回合砍掉",
            Skip::Blocked => "卡在提問，等人回答",
            Skip::UnknownStatus => "狀態不明，不確定它在不在忙",
            Skip::TurnInFlight => "還有一回合沒收掉",
            Skip::NeedsManualInstall => "新版還沒裝，要先手動安裝（見更新提示裡的指令）才能重啟套用",
            Skip::NoLongerPending => "排到它時已經不用重啟了（更新套用過或 run 不在了）",
            Skip::StateUnreadable => "讀不到它的狀態，這次沒動它；更新還在等，稍後再按一次",
            Skip::HostSuperseded => "主機設定已改變，沒有重啟新主機上的 bot",
            Skip::BackgroundJobs(_) => unreachable!("handled above"),
        })
    }
}

/// 非候選連「跳過」都不列，免得淹掉真正要看的那幾行。只有 claude／codex 會被 `update_watch` 寫
/// `update_notice`（grok 沒有這條巡邏），但這裡仍明講而不是「任何 kind 都算」，跟寫入端的假設對齊。
pub fn is_candidate(c: &Cand) -> bool {
    matches!(c.kind.as_str(), "claude" | "codex") && c.has_update
}

/// 這顆候選為什麼不能動；`None`＝可以重啟。順序即優先序，回報理由取第一個命中的（使用者最該先處理的那件）。
/// 計畫時用一次，**輪到它真的要重啟前再用一次**（[`run_batch`]）。
pub fn skip_reason(c: &Cand) -> Option<Skip> {
    if c.child {
        // 不是「暫時不能動」：子 agent 一律不由 daemon 重啟（SPEC §6.5a，2026-09-22）。
        Some(Skip::Child)
    } else if c.needs_manual_install {
        // 跟下面幾條「暫時不能動」不同，這條是「重啟了也沒用」，排最前面。
        Some(Skip::NeedsManualInstall)
    } else if c.default_session {
        // SPEC §6.5.1：重啟會關掉使用者自己的 pane（2026-09-12 review #4）。
        Some(Skip::DefaultSession)
    } else if c.state != "running" {
        Some(Skip::NotRunning)
    } else if c.agent_status == "working" {
        Some(Skip::Working)
    } else if c.agent_status == "blocked" {
        Some(Skip::Blocked)
    } else if c.agent_status != "idle" {
        Some(Skip::UnknownStatus)
    } else if c.turn_in_flight {
        Some(Skip::TurnInFlight)
    } else if let Some(n) = c.background_jobs.filter(|n| *n > 0) {
        // 排在最後：它是閒置、沒有回合，唯一還擋著的是背景工作。`None`（沒看過）不擋——沒有證據。
        Some(Skip::BackgroundJobs(n))
    } else {
        None
    }
}

/// 子 agent 列在跳過名單（`Skip::Child`）而不是候選：2026-09-12 曾把它們納入原地重啟，2026-09-22 rollout 證明
/// 那條路會把子 agent 弄到退役（herdr 回 agent_name_taken → 軟刪）；改回由父 bot 用 herdr 重開（SPEC §6.5a）。
pub fn plan(cands: &[Cand]) -> (Vec<&Cand>, Vec<(&Cand, Skip)>) {
    let mut go = Vec::new();
    let mut skip = Vec::new();
    for c in cands.iter().filter(|c| is_candidate(c)) {
        match skip_reason(c) {
            Some(w) => skip.push((c, w)),
            None => go.push(c),
        }
    }
    (go, skip)
}


/// 只重啟某台主機的某個 kind（`cli_update`：那台裝好 codex 新版之後，只重啟那台的 codex，不順手動到 claude 或別台）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub kind: String,
    pub host: String,
}

pub fn supervisor_last(mut targets: Vec<(String, String)>, supervisor: Option<&str>) -> Vec<(String, String)> {
    if let Some(sid) = supervisor {
        if let Some(i) = targets.iter().position(|(id, _)| id == sid) {
            let t = targets.remove(i);
            targets.push(t);
        }
    }
    targets
}
