---
name: verify
description: 在這個 repo 準備 git commit 程式碼改動之前執行：跑 `scripts/check.sh changed`，只驗改到的部分（L0/L1 快速閘）。Run right before committing code in agents-manager; check.sh itself skips docs-only changes.
---

# verify：commit 前的快速閘

commit 之前（含 worktree 裡），在**自己的 worktree** 跑這一行，這是唯一的驗證入口：

```bash
scripts/check.sh changed
```

- 它跟 `origin/main` 比（commit 差異＋還沒提交的改動），依改到的路徑決定跑哪些：web／daemon／ops／ob。
  判斷全在 `scripts/check.sh` 與 `scripts/ci-changed-parts.sh`，**這裡不重寫、也不要自己挑指令跑**。
- 只有文件類改動時它會印「只有文件類改動，不用跑」就結束；其他情況照跑，不自己判斷略過。
- 分支不是從 `origin/main` 切出來時，才多帶 base：`scripts/check.sh changed <base>`。
- daemon 做 `cargo check --all-targets`，再跑**改到的模組自己的測試**（`scripts/ci-daemon-filters.sh` 由路徑挑，例如改 `lifecycle/queue.rs` 跑 `lifecycle::queue::`）。要明講跑哪些：`CHECK_TESTS=<過濾字串> scripts/check.sh changed`；一個測試都不跑：`CHECK_TESTS=none`。
  不要為了「保險」跑整樹 daemon 測試、`scripts/flaky-sweep.sh`，也不要等 GitHub Actions。
- 整樹測試是另一道、非同步的關卡：Ubuntu 背景的 `ubuntu-ci`（commit status）會對 main HEAD 跑整樹 `scripts/check.sh`，**不在 commit 的 critical path 上**，推完就做下一件事。

## 結果怎麼處理

- **綠**：才可以 commit，回報時如實寫「`scripts/check.sh changed` 通過」。
- **紅或中途失敗**：不可宣稱已驗證、不可說 ready-to-commit。先修到綠；修不了就在回報裡寫出 blocker（哪個步驟、錯誤訊息）。
- 別人的 WIP 讓編譯掛掉、而你的改動本身沒問題時，照 repo `CLAUDE.md`「驗證」那段，對你 staged 的內容驗，不要碰別人的檔。

詳細規則見 repo 根目錄 `CLAUDE.md` 的「驗證」與「提交」。
