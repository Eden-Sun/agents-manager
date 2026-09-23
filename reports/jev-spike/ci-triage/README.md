# #266 Jev 分類 CI 失敗：離線評估

**結論：不要接成自動重跑，也不要接五選一的分類。只有一個二元問題「重跑會不會綠」值得留著，而且是純建議、掛在既有的 CI 盯哨上。**

日期 2026-09-24。模型 `jev-1.13.0`（TypeSafe System One）。48 次呼叫、0 錯誤、$0.0033。
不接 daemon、不改任何行為，只離線重放。

## 怎麼做的

- 樣本來自本 repo 的 GitHub Actions，2026-09-18 ~ 09-23 共 **400 個 run、157 個紅 run、285 個紅 job**。
  `collect.py` 抓 run／job／log，`build_cases.py` 做成題目。
- **人工標籤先於呼叫**（`labels.json`，判準寫在檔頭）。依據一律是可查證的事實：
  - 後續哪一個 commit 讓它變綠、那個 commit 改的是 production 還是測試；
  - 或 GitHub 上同一個 run 的 **attempt 2 結果**——這段期間有 5 筆真的被重跑，全部綠。
- 挑 24 題，三類各有代表：`real_regression` 6、`runner_environment` 7、`flaky_timing` 11。
  刻意放進 hard negative（連紅 128 次的長龍、看起來像環境問題其實是真 bug 的 cargo shim）。
- state 只放判斷當下拿得到的東西：job／失敗步驟／失敗測試名／`--log-failed` 摘錄（≤3000 字）／
  commit 標題與改到的路徑／**同一個 job 前 12 次的紅綠序列**與每條測試「在這次之前已經連紅幾次」。
  沒有任何未來資訊。log 先過一次憑據形狀的遮罩再送出。
- 兩組題目跑同一批 state：
  - **choice**：一題五選一（`real_regression` / `runner_environment` / `flaky_timing` / `infra` / `unclear`）。
  - **noul**：三題獨立機率（`rerun_would_pass` / `machine_specific` / `introduced_by_this_commit`）。

完整數字在 `score.txt`（`python3 score.py` 重算）。

## 結果

### choice（五選一）幾乎沒有訊息量

| 人工標籤 \ Jev | real_regression | runner_environment | flaky_timing |
|---|---|---|---|
| real_regression (6) | **6** | 0 | 0 |
| runner_environment (7) | 6 | **0** | 1 |
| flaky_timing (11) | 5 | 0 | **6** |

- 整體 acc **0.50**，全猜最大類的基準是 0.46。
- `runner_environment` recall **0/7**——這個類別 Jev 一次都沒選過，全部倒進 `real_regression`
  （precision 0.35）。而這正是本 repo 最大宗的紅因。
- **信心跟正確率是反的**：conf<0.5 時 acc 0.71，conf≥0.8 時 acc 0.43。
  `daemon-4e243799` 用 0.95 的信心說 real_regression，那一筆實際上重跑就綠了。
  信心值不能拿來當門檻。

### noul 三題：只有一題有用

| 問題 | n | acc@0.5 | 全猜多數 | Brier | p 的實際範圍 |
|---|---|---|---|---|---|
| `rerun_would_pass` | 24 | **0.75** | 0.54 | 0.176 | 0.07–0.58 |
| `machine_specific` | 24 | 0.71 | 0.71 | 0.258 | 0.04–0.45 |
| `introduced_by_this_commit` | 22 | 0.59 | 0.82 | 0.226 | 0.08–0.89 |

`machine_specific` 跟全猜「否」一樣（7 個真值它全給 ≤0.22）；`introduced_by_this_commit` 比全猜還差。
兩題都該丟掉。

`rerun_would_pass` 是唯一贏過基準的：6 個錯全是**偽陰性**（該重跑卻說不該），
**偽陽性 0 個**。方向是安全的那一邊（寧可升級也不要放過真的壞掉）。
但它的機率從沒超過 0.58——不是校準過的機率，只有**排序**可用，門檻要自己配。

### 「重跑一次」政策的精確率（#266 的升級門檻）

| 政策 | 判給重跑 | precision | recall | 誤放行的 |
|---|---|---|---|---|
| choice 說 flaky_timing | 7 | 0.86 | 0.55 | `0228e3f0` |
| noul `rerun_would_pass` ≥ 0.4 | 8 | 0.88 | 0.64 | `0228e3f0` |
| **noul `rerun_would_pass` ≥ 0.5** | **5** | **1.00** | **0.45** | 無 |
| 土法：所有紅測試上一個 run 還是綠的 | 15 | 0.67 | 0.91 | `0228e3f0`×2, `0276f68e`, `a7dc0b74`, `5a3cb5f7` |
| 土法 ∧ noul ≥ 0.5 | 5 | 1.00 | 0.45 | 無 |

5 筆「真的有人重跑而且綠了」的：noul≥0.5 命中 4/5，choice 說 flaky 也是 4/5。

**precision=1.00 是 5/5，不能當成「高精確率」**：0 次失手 / 5 次試驗的 95% 上界是 60% 失敗率
（rule of three），真實 precision 可能低到 0.4。樣本遠遠不夠支持自動化。

### 一個不需要模型的結論

285 個紅 job 裡，**只有 16 個（6%）是「新出現的紅」**；其餘 269 個的失敗測試在上一個 run 就已經紅了，
重跑必定無效。這 6% 用一行歷史查詢就篩得出來，不需要任何模型。
而且 255/285（89%）的紅 job 來自同一批 `runner_environment` 長龍（`scripts/ob_test.py` 連紅 128 次、
`api::shell` 兩條連紅 127 次、`supervisor::idle_sleep` 三條），157 個紅 run 有 128 個（82%）揹著它們。
**對這個 repo 來說，紅得最久的不是 flaky，是沒人發現的長龍**——盯哨（`3c781deb` 加的那條）
解掉的價值遠大於分類器。

### 成本

48 次呼叫、79 434 input tokens、3 024 output tokens、**$0.0033**；
延遲 choice p50 287ms / p95 508ms，noul p50 304ms / p95 359ms；0 次錯誤、0 次重試。
成本與延遲都不是障礙，瓶頸在標註。

## 值不值得接、接在哪、風險

**值得接的只有一條，而且是建議句，不是動作。**

- **接在哪**：既有的 CI 盯哨（`3c781deb` 加的「main 一紅就開 issue 派工」）在開 issue 時，
  多寫一行 `Jev: 重跑會綠的機率 0.xx（僅供參考，不是判斷）`。**不要**接在 workflow 裡、
  不要讓它決定要不要重跑、不要讓它影響派工優先序。
- **前置條件**：盯哨要先自己算「這條測試上一個 run 是不是綠的」。這件事免費、精確、
  而且擋掉 94% 的紅；Jev 只在剩下的 6% 上有話講。順序是先歷史、後模型。
- **不要接的**：五選一分類（acc 0.50，`runner_environment` 全盲）、`machine_specific`、
  `introduced_by_this_commit`、任何用 `confidence` 當門檻的設計（信心與正確率是反的）。

**風險**

1. **最大宗的紅因它看不見**。157 個紅 run 有 128 個（82%）是 `runner_environment`
   （測試依賴開發機上正在跑的東西、macOS 的 `/var`→`/private/var`），
   Jev 對這一類 recall 0/7，一律說成 `real_regression`。好處是方向安全（會升級、不會放過），
   代價是它對這個 repo 最痛的問題完全沒有貢獻。
2. **樣本裡沒有 `infra`**。400 個 run、6 天，一次 checkout／下載／網路失敗都沒有
   （`web` 與 `lint` job 從沒紅過）。這個類別等於沒測過，不能宣稱任何精確率。
3. **「重跑會綠」跟「沒有 bug」不是同一件事**。`daemon-4e243799` 重跑就綠，但根因是
   `promote` 的 production 競態，後來要 `dba66517` 改程式碼才根治。若把重跑當成結案，
   這種 bug 會被洗掉。所以 #266 寫的「不自動關 issue、不自動忽略」必須維持，
   而且重跑成功也要留下紀錄。
4. **機率不校準**。24 題全落在 0.07–0.58，0.5 這個門檻是從這 24 題挑出來的，
   換一批樣本會漂。要上線就得先有一個固定的、事前決定門檻的作法，並且持續對帳。
5. **只有 5 筆實測重跑**。其餘 19 筆的 `rerun_pass` 是推論來的（依後續修正 commit 的說明與
   前後綠 run），推論本身可能帶進跟 Jev 一樣的偏誤。

**要再往前走的話**，先做的不是換模型或改 prompt，是把 `--repeat` 這種「同一個 commit 重跑 N 次」
的資料收起來，讓 `rerun_would_pass` 有真的標籤可對；在那之前 precision 1.00 只是 5 筆。

## 檔案

- `collect.py`：從 GitHub API 抓 run／job／log 到 `<work-dir>`。
- `labels.json`：24 筆人工標籤、判準與依據（**在呼叫之前定好**）。
- `build_cases.py`：dump ＋ labels → `cases_choice.jsonl`／`cases_noul.jsonl`。
- `cases_*.jsonl`、`results_*.jsonl`：題目與 Jev 的回答（`../jev_replay.py run` 產生）。
- `score.py`／`score.txt`：混淆矩陣、每類 P/R、校準、重跑政策、逐筆對照。

重跑：

```sh
python3 reports/jev-spike/ci-triage/collect.py /tmp/ci-dump
python3 reports/jev-spike/ci-triage/build_cases.py /tmp/ci-dump
cd reports/jev-spike
TYPESAFE_API_KEY="$(cat ~/.config/typesafe/api-key)" python3 jev_replay.py run ci-triage/cases_choice.jsonl ci-triage/results_choice.jsonl
TYPESAFE_API_KEY="$(cat ~/.config/typesafe/api-key)" python3 jev_replay.py run ci-triage/cases_noul.jsonl ci-triage/results_noul.jsonl
python3 ci-triage/score.py
```
