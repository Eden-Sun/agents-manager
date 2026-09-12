# Goal：標題列／額度列收斂（ego 全畫面走查，2026-09-12）

執行者：cc2 · opus High（child `ag-man-z5qczr-ui`）。派工者：`ag-man-z5qczr`。
遵守 `Claude.md`：不要 stash、只 add 自己的檔、不要重啟 daemon、繁體中文回報。

輸入：ego 走查截圖 `docs/screenshots/ego-review/`（01–12）。桌機 1381px、手機 390×844。

## 不要翻案
- 左側 project 分組、右側聊天。
- 額度必須看得到**所有 kind**（claude 各身分、codex、grok），不能只留目前這顆 bot 的一格。
- 百分比始終保留，不能只靠顏色。
- `docs/UI-DECISIONS.md` 已定案的其餘項。新取捨補進去。

## 開工前
1. `git status`。工作樹常有其他 agent 的 WIP（`UnreadChip.tsx`、`ChatPanel.tsx`、`desktopBotHead.css`、`mobileBotHead.css`、`daemon/` 等）。**那些不是你的，不要 stash、不要 checkout、不要把別人的半成品一起 commit。**
2. 若修未讀列必須動到別人正在改的檔，hunk 要小，混檔用 `git add -p`／`git apply --cached` 只暫存自己的。

## 要做（依序，一個問題一個 commit）

### 1. 手機額度 chip：一格一個最急窗口
現況：cc1 同時 `7d 19%`＋`F 0%`，cc2 `7d 42%`＋`3m 0%`，字折行、列變高。
決策：每格常駐 **一個**數字（優先 7d／週；只有另一窗口是 low／critical 且比 7d 更急時才取代，不要兩個並列）。完整 5h／7d／Fable 仍點進底部 sheet。
檔案：`web/src/components/QuotaStrip.tsx`（compact `windows` 那段）、必要時 `mobileBotHead.css`。
驗收：390px 五格（cc0/cc1/cc2/codex/grok）都在、沒有一格折成兩行剩餘數字。

### 2. 未讀列不要吃掉對話
現況：桌機／手機「剛跑完」＋星星列兩排 chip，手機上蓋掉對話上緣；桌機把 git／context 擠到氣泡上。
決策：手機預設收成 **一列可橫捲**（跟 git 工具列同一種 scroll shadow），超過寬度就捲，不要第二排。目前這顆 bot 的 chip 置中或置左可見。桌機同樣最多一列；真的放不下才橫捲，禁止疊在 `.msg-list` 上。
檔案：未讀列相關（`UnreadChip.tsx` / `unreadChip.css` / header 裡掛它的地方）。動到共用檔 hunk 要小。
驗收：390 與 1380 寬，未讀列高度 ≤ 一列 chip；對話第一則不被蓋住；`scrollWidth === innerWidth`。

### 3. 桌機 git／context 列不得壓訊息
現況：`02-desktop-chat-clear.png` 裡 git／帳號／context 疊在氣泡上。
決策：context bar 是標題列底下自己的一列，不 `position` 蓋住 `.msg-list`。修完用 1380 與 1500 寬各截一張。

### 4. 不要做
- 不要重啟 daemon。
- 不要改 daemon／team 協定。
- 不要把額度改回「只顯示目前 kind」。
- 不要送訊息給使用者的 bot、不要停別人的 run。

## 驗證
- `cd web && bunx tsc --noEmit -p tsconfig.app.json && bunx oxlint src && bun run build`
- 手機：ego 或 `OUT=/tmp/mshots node scripts/ui-mobile-shots.mjs`，確認 390 無橫向整頁捲動。
- 截圖放 `docs/screenshots/header-chrome/`（桌機 chat、手機 chat、手機額度列、未讀列收斂後）。
- 補 `docs/UI-DECISIONS.md` 一小節。

## 回報
三到五行：commit hash、怎麼驗的、要不要重啟 daemon、沒做到的與原因。
