# #263 Jev 當「交辦回報證據檢查器」——離線重放結果

一句話：**中文不是瓶頸**。把同一則回報翻成英文再問一次，40 組判斷裡有 39 組一樣，
機率中位差 0.01。五個候選旗標裡 **`claims_verified`、`asks_parent_action`、
`contains_concrete_evidence` 可以接**（高門檻 precision 0.95～1.00），
`claims_complete` 只在開發／部署類回報上堪用，`admits_unfinished` 觸發率太高不值得接。
全程沒有改 daemon、沒有寫 DB、沒有動任何交辦狀態。

數字全在 [`analysis.md`](analysis.md)；這裡只講怎麼做的與要不要接。

## 樣本

40 則真實的子 agent 完成回報，從正式 DB 的 `supervisor_assignments.result` **唯讀**抽出
（母體：1,116 則長度 ≥60 的回報，其中 1,084 則含中文＝97%）。分三層抽，
每層照 ULID 尾碼排序取前 N，不是挑順眼的：

| 層 | 條件 | 取 | 代號 |
|---|---|---|---|
| 開發／部署 | 回報提到 commit／sha／CI／測試／cargo，且 AGM 驗收 `accept` | 23 | `D*` |
| 追加／被擋 | `review_decision` 是 followup／block／fail／cancel，或狀態 failed／blocked／cancelled | 10 | `F*` |
| 維運／確認 | 其餘（瀏覽器清理、收到確認、協調轉達） | 7 | `O*` |

語言分布跟母體一致：35 則繁中（CJK 佔字母比 0.29～0.82，中文散文夾英文識別碼）、
5 則整則英文。長度 78～2,864 字，中位數 747。

出門前做過三件事：掃金鑰（0 命中，只有「Bearer」這個字出現在散文裡）、
把別的專案／客戶側的名字換成代號（`build_cases.py` 的 `ALIASES`）、
**先標完人工標籤才呼叫 API**（`labels.json` 的時間戳早於 `results_*.jsonl`）。

## 五個問題與標註規則

每則問 5 個 Noul，題目與判準用英文寫（中英兩組都用同一份，語言才是唯一變因）。
`state` 只放 `report` 一個欄位——不放交辦原文，避免交辦裡的「做完要回報 sha 與 CI」
被讀成回報自己的宣稱。

| 旗標 | 標「真」的條件 | 刻意不算的 |
|---|---|---|
| `claims_complete` | 宣稱指派的工作（或主要交付物）已做完、已修好、已推、已上線 | 只是確認收到、只報進度、被擋住、主要交付物明說還沒好 |
| `claims_verified` | 宣稱至少一次測試／建置／型別檢查／CI **通過** | 說還在跑、沒跑、未確認 |
| `contains_concrete_evidence` | 有可查核的識別碼或量測（sha、CI run id、pid、通過數、備份檔名＋大小、前後數字） | 純散文、只有檔案路徑、只有計畫 |
| `asks_parent_action` | 需要收件者（派工者／使用者）動手、核准、裁示或回答 | 自己接下來要做的事、交代自己子 agent 的事、「本輪沒 build 沒重啟」這句 |
| `admits_unfinished` | 點名至少一個未了項（還沒部署、沒驗到、被額度／租約擋住、留給別人的缺陷、明說沒碰的部分） | 例行的「本輪沒 build／沒重啟」、只是解釋自己的設計取捨 |

真值分布（40 則）：complete 30/40、verified 22/40、evidence 34/40、asks 13/40、unfinished 27/40。
`contains_concrete_evidence` 的基準真值率就有 85%——AGM 的回報幾乎都帶數字，這件事本身
就讓這個旗標的資訊量有限。

## 中文準確度

這是這張票的核心，用兩種看法量：

1. **原生分布**：35 則繁中 vs 5 則整則英文。繁中 `claims_verified` acc 0.94、
   `contains_concrete_evidence` 0.97、`asks_parent_action` 0.86、`admits_unfinished` 0.86、
   `claims_complete` 0.71。英文那 5 則樣本太小，只能當參考。
2. **配對對照**（比較可信）：挑 8 則繁中回報逐句翻成英文，標籤不動，再跑一次。
   40 組判斷 **39 組一致**，唯一翻面的是 D03 的 `claims_complete`（中文 0.51 → 英文 0.35，
   本來就卡在門檻上）。機率絕對差中位數 0.01、p90 0.05、最大 0.31
   （D16 的 `claims_verified`，兩邊都判對）。

**結論：Jev 讀繁中報告沒有明顯折損，錯的地方中英文一起錯。**
`claims_complete` 的弱點跟語言無關：英文版 O41-en 一樣判 0.03、F31-en 一樣判 0.21。

## 每題的 keep / drop

門檻取 0.8（AGM 只想要高把握的提醒，不想要每則都閃）：

| 旗標 | p≥0.8 觸發 | precision | recall | 基準真值率 | 結論 |
|---|---|---|---|---|---|
| `claims_verified` | 21/40 | 0.95 | 0.91 | 0.55 | **接**。唯一的誤報 F32／F34 都是把「打算跑」讀成「跑過了」 |
| `asks_parent_action` | 7/40 | 1.00 | 0.54 | 0.33 | **接**。只在很確定時才閃，漏掉的 6 則多是「已申請核准」這種隱性要求 |
| `contains_concrete_evidence` | 34/40 | 1.00 | 1.00 | 0.85 | **接但沒什麼用**：基準就 85%，等於永遠是真。真要用，改問「有沒有 CI run id」用 regex 更準也更便宜 |
| `claims_complete` | 14/40 | 1.00 | 0.47 | 0.75 | **只接在開發／部署類**。整體 acc@0.5 只有 0.71，但誤報 0——10 個錯全是漏掉。維運／確認類（`O*`）acc 只有 0.43、機率中位數 0.06，Jev 不把「本輪清理報告」「收到，已納入計畫」當成完成宣稱 |
| `admits_unfinished` | 30/40 | 0.87 | 0.96 | 0.68 | **不接**。觸發 30/40，precision 0.87 對基準 0.68 幾乎沒有抬升，等於一個「幾乎總是亮」的燈 |

`claims_complete` 的 p<0.2 那一側反而不可信：14 則裡有 5 則其實是完成宣稱
（「為假」precision 只有 0.64）。其他四題的低機率側 precision 都是 1.00。
**所以低機率不能當「這則沒宣稱完成」的證據，只有高機率那側能用。**

## 矛盾偵測：難的是決定性事實那一半，不是 Jev

票上設想的兩條矛盾規則，實際跑起來：

- **「宣稱完成 × commit 不在 main」：這 40 則裡 0 則真命中。** 唯一兩則真的有 commit 不在 main
  的（D12 的 `fbc2442`、F27 的 `1b11090`），報告自己就寫了「未 push」「只留在本地」，
  而且 Jev 的 `claims_complete` 分別只給 0.36 與 0.09——真值與 Jev 都沒事，不需要這條規則。
- **更糟的是假警報來源**：報告裡的 7～40 碼 hex 有 7 個根本不是 git commit
  （sha256 前綴 `2ef08c0e…`、備份檔尾巴 `d7e1f3c2f5a7`、別的 repo 的 sha）。
  用 regex 抓 sha 再查 main，會在 4 則上無中生有地報「commit 不在 main」。
  這條線真要接，抓 sha 的規則得先做對（只認 `git cat-file -e <sha>^{commit}` 過得了的）。
- **「宣稱通過 × 沒有 CI run id」觸發 18/40**，但 AGM 的慣例本來就是本機跑 `cargo test` 不等 CI，
  所以這 18 則幾乎都是正常的。要接必須先把 `claims_verified` 拆成
  「宣稱本機測試過」與「宣稱 CI 過」兩題——這次沒量，是下一步。

## 建議怎麼接

1. **shadow 先接在 `supervisor::controller::settle()`**（回合結束、`result` 寫進交辦的那一刻），
   問 `claims_verified` 與 `asks_parent_action` 兩題，寫進一張跟 `judge_shadow` 同型的表
   （`assignment_id`、五個機率、model、ms、input_tokens）。一則一次呼叫、$0.00007、p95 502ms，
   對 10 秒的 TICK 沒有壓力。
2. **累積兩週後再談 UI**。第一個上線行為照票上寫的做**注意力標記**：
   在 `agm review` 與網頁的交辦卡上顯示「這則要你動手」（`asks_parent_action` ≥0.8），
   不做任何自動拒收、不改狀態。
3. **`claims_complete` 只在 `expects_review=1` 且交辦要求 sha 的那類交辦上問**，
   維運巡檢那類不要問。
4. **`admits_unfinished` 與 `contains_concrete_evidence` 先不要接線**，理由見上表。

## 風險

- **樣本偏一邊**：`contains_concrete_evidence` 85%、`claims_complete` 75% 都是真，
  precision 看起來漂亮有一部分是基準給的。要判斷「promotion criterion」，
  下一批樣本要刻意找**沒有數字的回報**與**宣稱完成但其實沒推的回報**，這兩類在真實歷史裡很少。
- **標籤只有我一個人標**，`claims_complete` 在維運類回報上的判準（清理報告算不算完成宣稱）
  跟 Jev 的讀法不同；這不是 Jev 讀錯中文，是題目沒定義清楚。真要上線前，
  這題的措辭要重寫成「報告是否宣稱**這一回合交辦的事**已經做完」並重標一次。
- **一則 `F34` 是髒資料**：那筆 `result` 其實是交辦內容加上終端機的額度橫幅，不是回報。
  Jev 在它身上 `claims_verified` 給 0.75、`admits_unfinished` 給 0.84，兩個都錯。
  接線前要先確認只餵真的回合回覆（`turn_status='completed'` 且來源是 hook／transcript）。
- **外流**：這條路會把整則回報送到 TypeSafe。這次已經遮掉別專案的名字，
  但正式接線要走既有的 judge 開關與每專案 opt-in（`docs/CHATGPT-CONSULT.md` 同一套規則），
  預設關閉。

## 檔案

- `labels.json`：40 則的人工標籤（先於呼叫）與 8 則翻譯配對名單。
- `build_cases.py`：從正式 DB 唯讀組題，遮名、擋金鑰；`--from/--translations` 組英文對照。
- `cases_zh.jsonl` / `cases_en.jsonl`：送出去的題目（含遮名後的回報全文）。
- `translations_en.json`：8 則的逐句英譯。
- `results_zh.jsonl` / `results_en.jsonl`：Jev 的原始回應（`jev-1.13.0`）。
- `facts.py` / `facts.json`：決定性事實（sha 解不解得出、在不在 main、CI run 結論）。
- `analyze.py` / `analysis.md`：precision／recall、中英對照、矛盾表。

重跑：`TYPESAFE_API_KEY="$(cat ~/.config/typesafe/api-key)" python3 ../jev_replay.py run cases_zh.jsonl results_zh.jsonl`
然後 `python3 analyze.py results_zh.jsonl results_en.jsonl facts.json`。
48 次呼叫、0 失敗、0 重試，input tokens 共 70,867，全部成本約 $0.003。
