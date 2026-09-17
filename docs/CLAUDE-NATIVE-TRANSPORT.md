# Claude Code native SendMessage/ListAgents 當 transport 的評估（issue #81）

**結論先講**：現階段不建議把它當成 herdr 之外的第二條 canonical transport，也不建議拿來
補強 herdr。不是因為它不能用，而是因為它解決的是**另一個問題**（同一台機器上、正在跑的
Claude session 之間互通消息），跟 agents-manager 需要的東西（daemon 主動、可證明、
可對帳、涵蓋 claude/codex/grok 三種 provider）在好幾個維度上對不上。細節、證據與哪些部分
以後可能有用寫在下面。

## 方法論

沒有對正在跑的 bot（七顆子 agent、使用者的入口 bot）送過任何訊息，也沒有為了測試另外開
Agent／fork（issue 交辦當下的硬性限制）。證據來源：

1. `SendMessage` 工具自己的說明文字（`ToolSearch("select:SendMessage")` 讀到的原文，逐句引用，
   不是用猜的）。
2. `ListAgents` 對現在這個 session 實際看到的環境跑了一次（唯讀，不送訊息）。
3. `claude --help` 的完整子命令清單。
4. herdr 那條路現有的實作：`daemon/src/lifecycle/delivery.rs`、`daemon/src/lifecycle/prompt.rs`、
   `daemon/src/herdr.rs`。
5. 一個 `#[cfg(test)]`（不進正式二進位）的小型原型：`daemon/src/lifecycle/native_transport_prototype.rs`，
   把「native 結果」套進既有 `Delivered`／`delivery` 欄位，用測試釘住套不套得上。

## 建議的抽象，跟這次評估的落點

```text
AgentMessageTransport
├─ ClaudeNativeTransport   ← 這次評估的對象
└─ HerdrPromptTransport    ← 現有實作，完全沒動
```

**第一個、也是最根本的發現**：`ClaudeNativeTransport` 不可能是 daemon 這個 Rust 行程直接持有
並呼叫的東西。`SendMessage`／`ListAgents` 是 Claude Code agent 在自己的工具呼叫回合裡才有的
能力；`claude --help` 列出的子命令（`agents`／`attach`／`logs`／`respawn`／`stop`／`rm`）
管的是背景 session 的生命週期，沒有一個是「從外部行程送一則訊息給任意 session」。herdr 那條路
之所以能被 daemon 直接呼叫，是因為 herdr 是一個 socket 服務，daemon 自己是 client；
native transport 沒有對等的東西——要嘛請某個活著的 agent session 代打（多一個 relay，
多一種「relay 自己活不活著」的失敗模式），要嘛教每顆 bot 自己在該用的時候呼叫 SendMessage
（等於把 transport 的決策權從 daemon 挪到 bot 自己身上，agents-manager 對「這則訊息送出去了
沒有」的 ownership 就弱掉了）。兩條路都比「換一個 transport 實作」大得多，不是這次原型能
解決的問題，先誠實記下來。

## 逐項對照「要驗證」清單

### cross-session delivery identity（怎麼定位對方）

- **ListAgents 看到的身分是 session 的顯示名稱**，不是 agents-manager 的 `bot_id`。實測
  （見上面「方法論」第 2 點）看到的名字（例如 `wt-is63web-9b`）恰好跟這台機器上那顆 bot 的
  worktree 目錄同名，但那是巧合，不是 agents-manager 設的——`git grep -n '"--name"\|"-n"'`
  daemon/src/lifecycle/start.rs 完全沒有結果：agents-manager 啟動 claude bot 時**從來沒有
  傳過 `--name`／`-n`**，顯示名稱完全交給 CLI 自己決定。要讓 native transport 能可靠定位到
  一顆特定的 agents-manager bot，第一步就要先讓 daemon 在啟動時傳穩定的 `--name`（例如
  `bot_id` 或 `<project>-<bot name>`），這是目前完全沒有的整合。
- 名字不保證存活到未來：`SendMessage` 自己的說明寫「latest wins」（同名的新 session 會蓋掉
  舊的定址），也提到需要 `[ref]` 消歧——這代表 identity 是「這一刻活著的某個東西」，不是
  像 `bot_id` 那樣的持久主鍵。
- **只涵蓋 claude**：`ListAgents`／`SendMessage` 是 Claude Code 專有的工具，codex／grok 的
  CLI 沒有對應的東西。agents-manager 明確支援三種 provider，一個只涵蓋 1/3 的 transport
  永遠不可能變成唯一的 canonical transport，最多是「claude↔claude 這條邊」的選項——這正是
  issue 本身的框架，這裡只是用實測確認沒有例外。

### bypass-permissions session 的 inbound approval/parking 行為

`SendMessage` 原文：「a session running in a different permission mode than yours holds
cross-session messages for its user's approval (and may let them expire)」。翻成白話：
**如果發送方跟接收方的 permission mode 不一樣，訊息會被接收方卡住等人核准，而且可能等到過期**。
agents-manager 的 autostart bot 常常跑在 `--dangerously-skip-permissions`（`bypass-permissions`）；
如果 daemon 或某個 relay agent 用**不同**的 permission mode 發送，訊息就可能卡住甚至消失，
而且卡住這件事只有本機才有 `[Cross-session delivery notice]` 回報——這對「自動化、不盯著人」
的派工管線是硬傷：現有 herdr 那條路完全沒有這種「因為身分／模式不同被人類卡住」的中間狀態。

### remote host / different identity 是否可用

- **可以跨機器**：`ListAgents` 的說明本身提到「on this machine, on another machine, or in
  the cloud」，SendMessage 也提到 Remote Control／cloud／Claude Desktop 這幾種對象——這是
  herdr 現在做不到的（herdr 要 SSH 轉發＋對方也跑 herdr server）。**但代價是完全沒有 ack**：
  原文「for a Remote Control, cloud or Claude Desktop session nothing reports back, so never
  treat silence as agreement」——連「有沒有被拒絕、有沒有被卡住等核准」這種本機才有的回報都沒有，
  等於黑箱丟訊息。拿掉可觀測性換來跨機器，對一個以「可證明送達」為賣點的系統（現有 herdr 那條路
  就是為了解決『herdr 回 ok 但字沒進去』這個真事故才做的）是倒退。
- identity（哪個帳號跑這個 session）：`ListAgents`／`SendMessage` 完全沒有暴露這件事，
  也沒有看到任何跟 identity／帳號綁定相關的欄位或警告——這代表 native transport 本身不管
  identity 這個維度，agents-manager 現有的「一個 bot 對一個 identity」模型完全要靠自己另外
  維護，native transport 幫不上忙也不會扯後腿。

### recipient restart/resume 後 message fate

`SendMessage` 原文：「Refer to agents by name — names keep working after an agent completes
(a send resumes it from its transcript)」。這代表**同一個 Claude Code harness 自己的
session/transcript**在完成後仍可以用名字接續。但這跟 agents-manager 的 restart 是**兩套完全
不同的東西**：daemon 重啟一顆 bot 是重新起一個 CLI 行程（`--resume <native_session_id>`，
見 `daemon/src/lifecycle/start.rs`），這個新行程在 Claude Code harness 眼中是不是「同一個
session」（因此 SendMessage 認不認得同一個名字接得回去）**沒有查證過，也查不到**——這需要
真的重啟一顆 bot 才能觀察，而這次任務明確禁止對正在跑的 bot 做這件事。誠實列成「未知」，
不假裝驗證過。

### 如何映射到現有 delivery=ok/unknown/failed

見 `daemon/src/lifecycle/native_transport_prototype.rs`（`#[cfg(test)]`，不進正式二進位）
與下面的表。三條測試釘住這個結論，不是只有文字：
`only_refusal_and_unknown_recipient_map_cleanly_onto_the_existing_four_delivery_values`、
`reusing_delivered_unproven_for_no_ack_would_wrongly_enable_auto_resend`、
`handed_and_reached_agree_on_auto_resend_even_though_reached_is_still_lossy`。

| native 結果 | 最接近的 `Delivered` | 保真度 | 為什麼 |
|---|---|---|---|
| `Reached`（送到 session，本機、沒被卡） | `Handed`（`ok`, `verified=false`） | **勉強** | `Handed` 好歹是 herdr RPC 同步回的『交給 agent 了』；`Reached` 只保證進了收件匣，對方 Claude 有沒有真的排進下一輪完全不知道。 |
| `ParkedForApproval`（本機，權限模式不同，等人核准，可能過期） | 無 | **套不上** | 不是 `pending`（那個字在這裡指『還沒送出、排佇列』，這筆已經送達）；不是 `ok`（可能永遠等不到、也可能過期）；`unknown` 最近但少了「有明確 expiry」這個維度，現有 schema 沒有這個概念。 |
| `Refused`（對方直接拒收） | `NotAttempted { retry: false }` | **準確** | 明確失敗，turn 該撤回，不是留著假裝『送過但失敗』的 `failed`。 |
| `NoAck`（遠端／cloud／Desktop，完全沒回報） | `Unproven`（`unknown`） | **勉強，而且有風險** | 直接借用 `Unproven` 會連帶借到它 `auto_resend=true` 的既有語意——那是因為 herdr 那條路『字沒進去才會重送』是安全的；`NoAck` 完全不知道對方收到沒有，自動重送有真的重複派工的風險。 |
| `UnknownRecipient`（名字對不到活著的 session） | `NotAttempted { retry: false }` | **準確** | 名字錯了不會自己變對，`retry` 沒有意義。 |

## 能／不能 對照

| 面向 | HerdrPromptTransport（現有） | ClaudeNativeTransport（native） |
|---|---|---|
| 誰是主動方 | daemon 自己（socket client） | 只有活著的 agent session；daemon 無法直接呼叫 |
| 涵蓋的 provider | claude／codex／grok | 只有 claude |
| 送達證明 | 有分級證據（transcript diff／echo row／RPC-only）；`Delivered` 五種結果各自對應清楚的重送策略 | 「到了」只到 session 收件匣層級；本機以外完全沒有後續回報 |
| 卡住等人核准 | 沒有這種中間態 | 有（權限模式不同時），且可能過期，現有四個 `delivery` 值裝不下 |
| 跨機器／跨帳號 | 需要 SSH＋對方也跑 herdr | 原生支援，但完全沒有 ack |
| 對 agents-manager 的可觀測性 | 全部經過 daemon，寫進 `turns`/`messages`，UI／歷史查得到 | 完全在 daemon 之外，agents-manager 對這個交換一無所知，除非額外教 agent 自己回報 |
| 需要新增整合工作 | 無（已在用） | 至少要：daemon 傳穩定 `--name`、決定 relay 架構、擴充 `delivery` 的狀態空間（等核准／過期）、驗證 restart 後身分是否延續 |

## 建議

- **不要**現在把 `ClaudeNativeTransport` 設成任何路徑的 canonical transport——`ParkedForApproval`
  跟 `NoAck` 這兩種結果套不進現有 `delivery` 語意，而且套錯（尤其是 `NoAck` 借用 `Unproven` 的
  `auto_resend`）有實際造成重複派工的風險，不是理論問題。
- 唯一可能值得的場景：**同一台機器上、都在互動模式、雙方都不是 `bypassPermissions`** 的
  claude↔claude 臨時協作（例如兩顆使用者自己盯著的 bot 互相通知），這個場景恰好避開了
  `ParkedForApproval`（權限模式相同不會被卡）跟 `NoAck`（本機才有回報）這兩個目前套不進去的
  結果，退化成只剩 `Reached`／`Refused`／`UnknownRecipient` 三種，都能(勉強或準確)映射；
  但這個場景範圍很窄，而且完全繞過 daemon 的可觀測性，不建議現在投資。
- 如果之後要重啟這個方向，下一步應該是：（1）驗證 daemon 重啟一顆 bot 後，native session
  的名字定址是否還接得回去（需要一個明確安排、使用者知情的重啟測試，不是這次順手做）；
  （2）決定 relay 架構要不要做，以及 relay 自己的存活誰來保證；（3）如果真的要收
  `ParkedForApproval`，要先在 `turns` 加欄位（例如 `native_pending_expires_at`），不是硬塞
  進現有四個值。

## 驗證

- `daemon/src/lifecycle/native_transport_prototype.rs`：`#[cfg(test)]` 整檔，`cargo test`
  才會編，`cargo build`／正式二進位完全看不到。3 條測試全綠（見上面「如何映射」小節列的測試名）。
- daemon 全套測試、`clippy --all-targets`：見 commit 訊息裡的數字。
- 沒有對任何正在跑的 bot 送過 `SendMessage`；沒有開過額外的 Agent／fork。
