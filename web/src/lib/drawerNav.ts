/**
 * 手機抽屜：點到側欄裡的 bot 列／專案頭就收抽屜（選取沒變的再點一次也要收）。
 * 用 capture 是因為收合鍵與 `⋯` 會 stopPropagation；會在抽屜裡開對話框的按鈕要留著抽屜，
 * 否則 `inert` 會把對話框一起關掉。
 *
 * 專家模式成員／群集列左邊的「管理者可用」勾選框（`.bot-manager-use-wrap`）只切換停用狀態，不是導覽，
 * 也要留在 STAY：否則點下去抽屜就收了（使用者 2026-11-04）。
 */
export const DRAWER_NAV = '.bot-row, .project-head'
export const DRAWER_STAY =
  '.bot-kids-toggle, .bot-actions, .bot-name-btn, .bot-name-input, .project-fold, .project-head-actions, .bot-manager-use-wrap'

/** 這個點擊目標該不該收抽屜。 */
export function drawerShouldClose(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) return false
  if (!target.closest(DRAWER_NAV)) return false
  return !target.closest(DRAWER_STAY)
}
