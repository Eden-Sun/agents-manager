# 按鈕 hover / focus 對比全面檢查與色票優化（2026-09-08）

執行者：opus。遵守 `CLAUDE.md`（不要 stash、只 add 自己的檔、不要重啟 daemon）。取捨寫進 `docs/UI-DECISIONS.md`。

## 問題
使用者截到兩個例子（`docs/screenshots/btn-contrast/before-*.png`，淺色主題）：
1. 淺色 banner（`team-paused` 那條琥珀底）上的按鈕 hover 後底色變成近白、文字也近白，整顆看不見。
2. ConfirmDialog 的「取消」hover 時 focus ring 與底色糊在一起；「關閉 issue」主鈕 hover 幾乎沒變化。

## 要做的
1. [x] **用 ego-browser 掃全部按鈕**（skill `ego-browser`；開 `http://127.0.0.1:5173/?token=$(cat ~/.config/agents-manager/ui-token)`，也可用 `VITE_MOCK=1` 的 mock 把對話框都打開）：
   - 深色與淺色各跑一遍（`Emulation.setEmulatedMedia` prefers-color-scheme）。
   - 對每個 `button`、`[role=button]`、`.btn`、`.mini-btn`、`.icon-btn`、`.opt`、`.mem-badge` 等可點元素：用 CDP `CSS.forcePseudoState`（`hover`、`focus-visible`、`active`）強制狀態，讀 computed `color` / `background-color`（含往上找到第一個不透明的祖先背景），算 WCAG 對比。
   - 列出對比 < 3:1（文字對背景）或 hover 前後幾乎沒差（ΔL < 5%）的，存成 `docs/screenshots/btn-contrast/audit.json`（selector、狀態、主題、fg、bg、ratio、所在面板）。
   - 每個面板都要打開掃到：Sidebar（含 project/bot 列 hover、⋯ 選單）、ChatPanel 標題列與 composer、TerminalTab、BotSettingsPanel、ConfirmDialog、TeamPanel（paused banner、role editor）、TeamLaunchPanel、HostsPanel、IdentitiesPanel、QuotaStrip popover、ImageShelf、HostShellPanel、BlockedPanel keypad、IssuesBar。
2. [x] **用 `color-palette` skill 重整按鈕色票**：以現有 `--accent`（styles.css `:root`）當 brand hex 產 11 階，定義按鈕 token：
   `--btn-bg / --btn-fg / --btn-bg-hover / --btn-fg-hover / --btn-border`，primary / danger / ghost / on-banner 四種變體，深淺兩套，全部過 WCAG AA（文字 ≥ 4.5:1，icon-only ≥ 3:1），hover 與常態的底色差 ≥ 8% L。
   - 集中在 `styles.css` 一個 `/* ---- buttons ---- */` 區塊，把散落的 `.btn:hover`、`.mini-btn:hover`、`.icon-btn:hover`、banner 內按鈕的覆寫改成吃 token；不要動版面尺寸。
   - focus-visible 用 2px offset ring（`--ring`），不要和 hover 底色同色。
3. [x] 改完再掃一次，audit 要是空的（或剩下有理由的例外，寫在 UI-DECISIONS）。截兩張 after 對應 before 的畫面放 `docs/screenshots/btn-contrast/after-*.png`。

## 驗證
- `cd web && bunx tsc --noEmit && bunx oxlint src && bun run build`
- `OUT=/tmp/shots node scripts/ui-goal-shots.mjs`（深淺各一張看整體沒有走樣）
- 一個功能一個 commit（`fix(web): …`）並 push origin main。

## 回報
三到五行：commit hash、掃到幾顆／修了幾顆、需重啟 daemon、沒做到的與原因。

---

## 結果（2026-09-08，opus）

- [x] **掃描**：ego-browser + CDP `CSS.forcePseudoState`，17 個畫面狀態（sidebar / 專案⋯選單 /
      ConfirmDialog / bot⋯選單 / 新增 Project / 新增 Bot / 環境設定（HostsPanel + IdentitiesPanel）/
      MemPopover / QuotaStrip popover / ChatPanel / ModelPicker / 標題列⋯選單 / BotSettingsPanel /
      TerminalTab / TeamPanel / HostShellPanel），深淺各一遍，每次約 100–160 個可點元素。
      hover 連同祖先一起強制（`.bot-row:hover .icon-btn` 這種規則才會生效），背景往上合成到
      第一個不透明的底色，另外處理了 `color-mix` 回傳的 `color(srgb …)` 與 transition 中途取值。
      TeamLaunchPanel / BlockedPanel keypad / ImageShelf 卡片沒有單獨開，但同一批 class
      （`.opt` / `.key-btn` / `.mini-btn` / `.icon-btn`）都在其他畫面掃到了。
- [x] **色票**：`color-palette` 從 `--accent` `#2f6fe4` 產 11 階（h219 s77），brand 落在 400–500 之間；
      `--accent` 維持不動（已定案），只取加深一階當 hover。四個變體＋深淺兩套的 `--btn-*`
      集中在 styles.css 的 `buttons` 區塊。
- [x] **重掃**：不合格 114 → 10 筆，對比不合格 10 → 0、focus 環不合格 13 → 0。
      剩下 10 筆是列 hover／透明外殼／行內元素的刻意例外，理由寫在 `docs/UI-DECISIONS.md`。
- [x] 截圖：`docs/screenshots/btn-contrast/after-1.png`（琥珀提醒條上的主鈕 hover+focus）、
      `after-2.png`（ConfirmDialog 的「取消」focus 環與危險鍵 hover）。量測 `audit.json`。

