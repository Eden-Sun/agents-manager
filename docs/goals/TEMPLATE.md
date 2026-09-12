# Goal template

執行者：<agent 名稱>。遵守 `CLAUDE.md`；一個邏輯段落一個 commit，完成後在本檔勾選進度。

## 開工前

- 先跑 `git status`，不要碰其他 agent 的未提交改動。
- `git worktree list` 找自己的 `.claude/worktrees/<你的 agent 名>`；沒有就從主樹建立：
  `git worktree add .claude/worktrees/<你的 agent 名> -b <分支>`。
- 只在自己的 worktree 工作；不要在主樹 `git stash`、`--autostash` 或 `git checkout --`。

## 工作規範

- 只改任務需要的檔案，只 `git add` 自己的檔案與 hunk，不要把別人的改動帶進 commit。
- commit 後回報 commit hash；主樹只由派工者用 `git -C <主樹> merge --ff-only <你的分支>` 整合。

## 驗證與收尾

- 收尾前跑 `scripts/check.sh`，並記下 build/test 的數字。
- 完成後移除自己的 worktree：`git worktree remove .claude/worktrees/<你的 agent 名>`。

## 回報

```text
report：status（done 或 blocked）、commit hash、改了什麼、怎麼驗的、未完成項目與原因。
```
