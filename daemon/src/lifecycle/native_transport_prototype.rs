//! GH #81 探索用的原型：Claude Code native `SendMessage`/`ListAgents` 能不能當
//! Claude↔Claude 的 transport，套進 herdr 那條路已經在用的 `turns.delivery`
//! （pending/ok/unknown/failed）語意。
//!
//! **整個檔案只在 `cargo test` 底下編**（`lifecycle/mod.rs` 用 `#[cfg(test)] mod
//! native_transport_prototype;` 掛進來），不會出現在 `cargo build`／正式二進位裡——這是刻意的，
//! 對應 issue #81「不要把原型接進生產路徑」。這裡驗證的是**抽象與映射邏輯站不站得住腳**，不是真的
//! 呼叫 SendMessage（那是 Claude Code 的工具，只有一個正在跑的 agent session 呼叫得到，這個
//! daemon 行程本身沒有管道呼叫它——這正是下面第一個結論）。完整分析、量到的證據與建議見
//! `docs/CLAUDE-NATIVE-TRANSPORT.md`。
//!
//! # 為什麼 `AgentMessageTransport` 不能是 daemon 直接持有並呼叫的東西
//!
//! herdr 那條路（`HerdrPromptTransport`，見 `delivery.rs`／`prompt.rs`）是 daemon **自己**打開
//! socket、自己打字、自己讀 pane 證明送達——daemon 全程是主動方，不需要任何一個 agent session
//! 配合。`ClaudeNativeTransport` 做不到這件事：`SendMessage`／`ListAgents` 是 Claude Code
//! agent 在自己的工具呼叫回合裡才有的能力，`claude` CLI 沒有對應的子命令或 socket API 讓外部
//! 行程（這個 daemon）代打（`claude --help` 列出的子命令只有 `agents`／`attach`／`logs`／
//! `respawn`／`stop`／`rm` 這幾個管背景 session 生命週期的，沒有「送一則訊息到任意 session」
//! 這種原語）。若真的要接，daemon 只能請某個活著的 Claude session（例如發訊息那顆 bot自己，
//! 或另外常駐一個 relay agent）代為呼叫，而不是像 herdr 一樣直接控制——這是比「換一個
//! transport」更大的架構改動，而且那個 relay/發訊息的 agent 本身要嘛跑著、要嘛不跑，
//! 多了一種新的失敗模式。

use super::*;

/// `SendMessage` 工具自己說明文字寫清楚的結果（`ToolSearch("select:SendMessage")` 讀到的，
/// 不是用猜的）：
/// - 「成功送出」只代表訊息到了那個 session，不代表對方的 Claude 讀了。
/// - 本機的 session：對方權限模式跟你不一樣時，訊息會被留著等使用者核准，而且可能等到過期。
/// - 對方可以直接拒收。
/// - 遠端（Remote Control／cloud／Claude Desktop）的 session：上面幾種狀況一律不回報，
///   工具說明原句是「never treat silence as agreement」。
/// - 名字要嘛對到一個活著的 session、要嘛對不到——對不到時直接是呼叫端的錯誤，不會排隊等。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeSendOutcome {
    /// 到了那個 session（本機、對方沒有權限模式差異、也沒被拒）。
    Reached,
    /// 本機 session，因為權限模式不同被留著等人核准；可能會過期。
    ParkedForApproval,
    /// 對方直接拒收。
    Refused,
    /// 遠端／cloud／Desktop：送出去之後完全沒有後續回報。
    NoAck,
    /// 名字對不到任何一個活著的 session。
    UnknownRecipient,
}

/// 這個 native 結果套進既有 `Delivered`（`turns.delivery` 只認 pending/ok/unknown/failed）
/// 準不準。三種等級：
/// - `Exact`：語意跟某個既有變體完全一樣，套上去不會誤導任何人。
/// - `Lossy`：勉強套得上某個既有值，但會丟掉一些既有語意原本保證的東西（見 `loses`）。
/// - `NoFit`：四個值沒有一個是對的，硬套任何一個都是在講謊話；要嘛加新狀態、要嘛加新欄位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MappingFidelity {
    Exact(&'static str),
    Lossy { closest: &'static str, loses: &'static str },
    NoFit { why: &'static str },
}

pub(crate) fn classify(outcome: NativeSendOutcome) -> MappingFidelity {
    match outcome {
        NativeSendOutcome::Reached => MappingFidelity::Lossy {
            closest: "ok, verified=false（跟 Delivered::Handed 同一級）",
            loses: "Handed 至少是 herdr agent.prompt 那個 RPC 同步回的『herdr 說它把字交給 agent 了』；\
                    Reached 只保證訊息進了對方 session 的收件匣，對方的 Claude 有沒有真的在下一輪讀到、\
                    有沒有排進佇列，這個結果本身完全沒說。",
        },
        NativeSendOutcome::ParkedForApproval => MappingFidelity::NoFit {
            why: "四個值都不對：不是 pending（這個 schema 裡 pending 指『還沒送出、排在佇列』，\
                  這一筆明明已經送達）；不是 ok（可能永遠等不到使用者核准，也可能過期，當作送達會誤導\
                  下游——例如 stall watchdog 會以為已交給對方，不會再提醒誰去按核准）；unknown 最接近，\
                  但少了『這筆有明確 expiry，過期後狀態要跟著變』這個維度，現有 schema 沒有這個概念。",
        },
        NativeSendOutcome::Refused => MappingFidelity::Exact(
            "NotAttempted { retry: false }（明確失敗，turn 要撤回，不是留著假裝『送過但失敗』的 failed）",
        ),
        NativeSendOutcome::NoAck => MappingFidelity::Lossy {
            closest: "unknown（跟 Delivered::Unproven 同一級）",
            loses: "Unproven 的 record() 把 auto_resend 定死成 true——那是因為 herdr 那條路『keys 送出去了、\
                    只是證不出』時重送是安全的（字沒進去才會重送，見 delivery.rs 的註解）。NoAck 完全不知道\
                    對方收到沒有，直接借用 Unproven 會不小心打開一個真的可能造成重複派工的 auto_resend。",
        },
        NativeSendOutcome::UnknownRecipient => MappingFidelity::Exact("NotAttempted { retry: false }"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 對應 docs/CLAUDE-NATIVE-TRANSPORT.md 的「能／不能」表：只有明確失敗（拒收／名字不存在）
    /// 套得上既有四個值而不丟語意；「到了」跟「完全沒回報」都是勉強套（丟掉一些既有保證）；
    /// 「等人核准、可能過期」現有 schema 完全沒有對應的位置。
    #[test]
    fn only_refusal_and_unknown_recipient_map_cleanly_onto_the_existing_four_delivery_values() {
        assert!(matches!(classify(NativeSendOutcome::Refused), MappingFidelity::Exact(_)));
        assert!(matches!(classify(NativeSendOutcome::UnknownRecipient), MappingFidelity::Exact(_)));
        assert!(matches!(classify(NativeSendOutcome::Reached), MappingFidelity::Lossy { .. }));
        assert!(matches!(classify(NativeSendOutcome::NoAck), MappingFidelity::Lossy { .. }));
        assert!(matches!(classify(NativeSendOutcome::ParkedForApproval), MappingFidelity::NoFit { .. }));
    }

    /// 具體釘住「為什麼不能直接借用 Unproven 給 NoAck」這個結論，不是只有文字說明：如果
    /// `Delivered::Unproven` 哪天不再固定開 `auto_resend`，這條測試會轉紅，代表上面 `NoAck` 那則
    /// 分析要跟著重寫，而不是悄悄過期。
    #[test]
    fn reusing_delivered_unproven_for_no_ack_would_wrongly_enable_auto_resend() {
        let record = Delivered::Unproven("native_no_ack").record().expect("Unproven 一定有 record");
        assert!(
            record.auto_resend,
            "Delivered::Unproven 的 auto_resend 語意變了——native_transport_prototype 裡『借用 Unproven 不安全』\
             這個結論是建立在『Unproven 一律 auto_resend=true』上，這裡也要跟著重新評估"
        );
    }

    /// `Reached` 借用 `Handed` 至少在 `auto_resend` 這個維度上是安全的：兩者都是「沒有無損證據，
    /// 但來源本身認為重送不會造成重複派工」（herdr 的 `agent.prompt` RPC／native 的 session
    /// 收件匣都一樣，沒有『已經打進 pane、重送會變成打兩次』那種顧慮）。這條測試釘住這個前提；
    /// 「Reached 仍然是 Lossy」是因為 `verified` 語意的落差（見上面 `classify` 的說明），跟
    /// `auto_resend` 無關。
    #[test]
    fn handed_and_reached_agree_on_auto_resend_even_though_reached_is_still_lossy() {
        let handed = Delivered::Handed.record().expect("Handed 一定有 record");
        assert!(
            handed.auto_resend,
            "Delivered::Handed 的 auto_resend 語意變了——上面這條測試名稱說的『兩者一致』要重新評估"
        );
    }
}
