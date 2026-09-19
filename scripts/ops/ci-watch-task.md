AGM 定期交辦：main 的 GitHub CI 紅了（`ci-watch-kick.sh` 偵測到並開了 issue，內容在訊息末尾）。請修到 GitHub 上的 CI 轉綠，不是只讓本機 `scripts/check.sh` 綠。

## 先判定
每條失敗的測試先分清楚：
- **真 bug**：本機 `cargo test -p agents-managerd <測試名>`（或 `scripts/check.sh ob`）也紅 → 修程式。
- **只在 runner 上紅**：本機綠。runner 是 macOS（GitHub hosted）、`TZ=Asia/Taipei`、claude／codex／grok 是空殼、沒有 herdr、沒有 `AM_*`、沒有真的 pane 行程。要在本機用 `env -i`、拿掉 `AM_*`、把外部指令換成會失敗的假貨重現，不要靠猜。環境相依的東西改成可注入，讓測試在哪都驗同一件事。
- **禁止** `#[ignore]`、刪斷言、放寬斷言換綠燈；真的只能在特定機器跑的，寫明原因、用具體條件在 CI 略過，並至少留一條不依賴環境的版本。

## 嫌疑與去重
- issue 內文有第一個紅的 run 與上一個綠之後的 commit 清單，先看那幾個。
- 動手前先看 `git log origin/main` 與在跑的子 agent，別人可能已經在修同一條。
- 同一段紅只有這一張 issue；後面又多的失敗會在同一張留言。**不要另開新的**。

## 收尾
- 推完用 `gh run list --branch main --commit <sha>` 找到那次 run，`gh run watch <id>` 盯到跑完，確認你負責的那幾條不在失敗清單裡。還在就繼續修。
- 紅的不是你造成、也修不了：在 issue 留言指出哪一條、哪個 run，回報派工者。
- 全綠後在 issue 留言驗證用的 run id，**由你關 issue**（盯哨不會自動關）。
- 全程繁體中文；遵守 repo 的 `CLAUDE.md`（自己的 worktree、不重啟 daemon、只 add 自己的檔）。
