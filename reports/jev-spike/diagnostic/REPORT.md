# #267 Jev 當「bot 卡住時的診斷第二意見」——離線重放結果

**結論：不要接。** Jev 的 `next_action` 在 49 個真實事件上只有 **0.51**，而且錯的方向正好是
#267 自己列為擋關條件的那個：**誤勸重啟 13 筆（restart_bot precision 0.24）**，其中 8 筆是
根本不該講話的雜訊。唯一值得留的是兩個 noul——**「是不是額度問題」0.94、「是不是登入問題」0.96**，
校準也乾淨——但這兩題 AGM 現在的欄位就答得出來，Jev 沒有多給什麼。

跑的是 `jev-latest`（服務端回報 `jev-1.13.0`），49 次呼叫、0 失敗、p50 282ms、p95 478ms、
輸入 49,376 token、輸出 7,476 token、成本約 US$0.002。原始資料在 `cases.jsonl` / `results.jsonl`。

## 怎麼做的

- 樣本 49 筆。40 筆從本機 `daemon.log`（2026-09-07 ~ 09-23）抽，每筆是同一顆 bot／主機
  在事發前 20 分鐘的維運訊息視窗，最後一行是觸發行；**視窗只含事發之前**。
  9 筆來自 `GET /api/supervisor/inbox?all=1` 與 `/incidents?all=1`。
- 遮罩：帶對話內容的 `hook received` 整條不取；email／UUID／ULID／sha／IP／家目錄都換佔位字串，
  bot 名字換成 `bot-1`。輸出前用 regex 掃 token 樣態，有就中止。送出的 49 筆已掃過，無殘留。
- 題目：1 題 choice（`next_action`，五選一）＋ 5 題 noul（`quota_problem`、`auth_problem`、
  `needs_human`、`self_clearing`、`restart_helps`），一筆一次呼叫。
- 重建：`python3 build_cases.py ~/.config/agents-manager/daemon.log > cases.jsonl`，
  `JEV_MODEL=jev-latest TYPESAFE_API_KEY=… python3 ../jev_replay.py run cases.jsonl results.jsonl`，
  `python3 analyze.py cases.jsonl results.jsonl`。

### 標註規則

標籤是我（i267）在呼叫 Jev 之前定的，依據是**事發後 60 分鐘的 log**——Jev 看不到那一段，我看得到。

| 標籤 | 什麼時候給 |
|---|---|
| `wait_quota` | 擋住的是帳號額度，等重置（或把工作 park 到重置）就好 |
| `ask_login` | 要人把 CLI 登進去、或指到對的帳號，之後才動得了 |
| `restart_bot` | agent 或它的 pane 卡死／不在了／沒接回來，下一步就是再起一次 |
| `escalate_user` | 要人看了才能決定：不是額度、不是登入，重啟也修不好 |
| `no_action` | 不該建議任何事：暫時的、預期內的、daemon 自己在處理的、或根本是誤判 |

分布：`no_action` 16、`escalate_user` 13、`restart_bot` 8、`wait_quota` 7、`ask_login` 5。
最大類 16/49 = **0.33**，這是「什麼都不判、一律說沒事」的底線。

## 數字

### next_action（五選一，n=49）

| | acc | 備註 |
|---|---|---|
| Jev | **0.51** | 25/49 |
| 只猜最大類 | 0.33 | |
| 關鍵字規則表 | 0.94 | **見下面的警告，這個數字不能直接信** |

逐類：

| 類別 | Jev precision | Jev recall |
|---|---|---|
| `wait_quota` | 0.70 (7/10) | 1.00 (7/7) |
| `ask_login` | 0.83 (5/6) | 1.00 (5/5) |
| `restart_bot` | **0.24 (4/17)** | 0.50 (4/8) |
| `escalate_user` | 0.67 (6/9) | 0.46 (6/13) |
| `no_action` | 0.43 (3/7) | **0.19 (3/16)** |

預測分布 vs 真實分布：Jev 說 `restart_bot` 17 次（真的只有 8 次），說 `no_action` 7 次（真的有 16 次）。
**它系統性地把雜訊讀成「這顆壞了，重啟」。**

### 誤勸重啟的 13 筆

`na-codex-history`、`na-subdrop`、`na-maint-gone`、`na-hostconn`、`na-sleep`、`na-update-restart`、
`na-stall-busy`、`na-grok-park`（真值都是 `no_action`）；
`es-undeliv-2`、`es-queue-too-long`、`es-stall-screen`、`es-paste-head`、`es-qback-transcript`
（真值都是 `escalate_user`）。

而且是**有把握地錯**：

| conf | 題目 | Jev 說 | 真的是 | 那一行其實是什麼 |
|---|---|---|---|---|
| 0.95 | `na-update-restart` | restart_bot | no_action | daemon 正在做 claude 換版重啟，4 秒後就 `restarted for the claude update (resumed)` |
| 0.94 | `na-sleep` | restart_bot | no_action | 同一秒的下一行是 `idle bot put to sleep`——是刻意休眠 |
| 0.90 | `na-hostconn` | restart_bot | no_action | 遠端主機第一次連不上，5 秒後自己接上了 |
| 0.88 | `na-codex-source` | wait_quota | no_action | codex 畫面上的「撞限」字樣結尾是 `",`、日期是 2025 年——那是原始碼，不是橫幅 |
| 0.87 | `sup-inc-stalled` | escalate_user | no_action | incident 30 秒後就自己 resolved |
| 0.85 | `es-stall-screen` | restart_bot | escalate_user | daemon 自己說的是「請查看終端分頁」 |

### noul

| 題目 | acc@0.5 | brier | 判讀 |
|---|---|---|---|
| `auth_problem` | **0.96** | **0.049** | 可用。p<0.2 那 32 筆全部真的不是登入問題；p>0.8 那 3 筆全中 |
| `quota_problem` | **0.94** | **0.053** | 可用。p<0.4 那 38 筆全部真的不是額度；p>0.8 那 8 筆中 7 筆是 |
| `restart_helps` | 0.78 | 0.201 | 不能用。8 筆真的該重啟裡只抓到 2 筆，另外還多出 4 筆偽陽 |
| `self_clearing` | 0.69 | 0.193 | 不能用。49 筆裡有 47 筆落在 p<0.6，等於從不表態 |
| `needs_human` | 0.55 | 0.285 | 不能用。比「一律回否」（31/49 = 0.63）還差 |

`needs_human` 當上升門檻的話：th=0.5 precision 0.44 / recall 0.83；拉到 0.8 precision 掉到 0.33。
**任何門檻下有一半以上的上升是假的**，這顆當不了「要不要叫人」的閘門。

唯一的 quota 偽陽 `na-codex-source` p=0.94，正是 #264 那個題目——畫面上印出來的原始碼被當成活的
UI 元件。額度那條線之所以看起來乾淨，是因為 49 筆裡這種陷阱只放了 3 筆。

### 和規則表比：那個 0.94 是假的

`analyze.py` 裡的 `rules()` 是我**看過標籤之後**才寫的關鍵字表，而且——更關鍵——
**我的標籤本來就大致是「哪一行 log 響了」的函數**（`logged_in=Some(false)` → 登入、
`等額度回來` → 額度、`restart for the claude update failed` → 重啟）。
所以規則表 0.94 幾乎是套套邏輯，不是「現成規則有多強」的公允估計，
這個比較**對 Jev 不公平**，不該拿它當「Jev 輸給規則」的證據。

真正能說的是拆開來看的那一段：兩邊都對 23 筆、只有規則對 23 筆、**只有 Jev 對 2 筆**
（`rs-stuck-pane`、`rs-enter-loop`——都是「pane 卡死」，也都是它那個重啟偏誤剛好猜對）、
都錯 1 筆。**Jev 沒有補到任何一類規則看不見的情況。**
把兩者串起來（規則沒話說才問 Jev）反而把 0.94 拉低到 0.73，因為 Jev 會把規則正確判成
`no_action` 的雜訊改判成重啟。

### 依 CLI 分

claude 20/36 = 0.56、codex 2/5 = 0.40、grok 0/1 = 0.00、unknown 3/7 = 0.43。
codex 與 grok 的樣本太少，只能說沒有哪一種明顯比較好。

## 這份實驗自己的毛病（先講在前面）

1. **標籤和觸發訊息高度相關。** 如上，這讓規則表的分數虛高。Jev 的 0.51 不受這點影響
   （它看的是同一批證據），但「Jev vs 規則」這個比較要打折。
2. **`restart_bot` 的選項描述可能誘導。** 我寫的是「agent 或它的 pane 卡死、不在了、或沒接回來」，
   而雜訊行的字面就是 `agent gone`、`pane closed forcibly`、`subscription dropped`。
   重啟偏誤有多少來自措辭、多少來自模型，這次**沒有測**——要斷定得再跑一輪換過措辭與選項順序的
   variant，本次呼叫預算（≤50）用完了。這是本結論最大的威脅。
3. **證據被我砍過。** `prompt stalled` 那類訊息原本會附整片終端畫面，我為了遮罩把多行內容整段丟掉，
   所以 `es-stall-screen` 這種題目比 AGM 真正會看到的難。真要接線時 Jev 拿得到畫面，表現可能不同。
4. **類別不平衡且樣本小。** 49 筆、最小類 5 筆，逐類 precision／recall 的信賴區間很寬。
5. **只有一台機器、一個使用者的 log。** 跨帳號／跨主機的情境（remote host 只有 5 筆）覆蓋不足。

## 值不值得接、接在哪、風險

**不值得接進建議路徑。** #267 自己寫的升級門檻是「correct next-action rate 要變好、
false restart/switch 要變少」。這次量到的是 next-action 0.51、誤勸重啟 13/49，兩條都沒過。
以 AGM 的使用量（光 `codex limit banner is history` 這一種訊息 log 裡就有 14,966 行、
`pane status subscription dropped` 3,333 行），一個 `no_action` recall 只有 0.19 的模型
會把巡檢的 inbox 灌爆，而且每一筆都附著一個看起來很有把握的「重啟這顆」。

**真要接，只能接這兩點，而且都只當 shadow 欄位：**

- `quota_problem` / `auth_problem` 兩個 noul，寫進 `supervisor` 的事實欄位旁邊，**不產生建議文字**，
  只當人在看 incident 時的第二欄。門檻建議取兩端：p<0.2 與 p>0.8 才顯示，中間留白——
  這次中間那幾桶幾乎沒有訊息量。
- 前提是先解掉 `na-codex-source` 那一類：**送進去的畫面要先過 #264 那道「這是活的 UI 還是印出來的字」**，
  否則額度那條線的乾淨是假的。

**不要接的：** `next_action` choice、`needs_human`、`restart_helps`、`self_clearing`。
尤其 `restart_helps` 和真實情況是反的（8 筆該重啟只抓到 2 筆，卻多出 4 筆偽陽），
把它接上「建議重啟」的路徑會直接製造 #267 安全章節要防的那件事。

**風險：**

- 重啟是不可逆的：一次誤勸重啟會丟掉一顆 bot 的上下文。這次 49 筆就有 13 筆會勸錯。
- 每筆要送 20 分鐘的 log 視窗出去。這次遮罩是逐行 regex，**多行畫面一律丟掉才安全**；
  真要接線得把遮罩寫進 daemon 並補測試，不能沿用這支 spike 腳本。
- 延遲與成本不是問題（p95 478ms、49 筆 US$0.002）。擋關的是準確率，不是預算。

**接下來該做什麼（如果還要再看一次）：** 先跑「措辭 variant」那一輪——同一批 49 題，
把 `restart_bot` 的描述改成不含 `gone` / `closed` 這些字面詞、選項順序打散，
看重啟偏誤掉多少。那一輪之前，這張票的答案就是不接。
