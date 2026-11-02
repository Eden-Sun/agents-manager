/**
 * 主機 shell 的 xterm 字級（純函式）。
 *
 * - 預設（`fitScreen` 關）：字級固定在可讀的下限以上（12px）。pane 放不下就在盒子裡捲動，不為了塞進去把字縮成看不清。
 * - 縮放至全螢幕（`fitScreen` 開）：縮到整個 pane（cols×rows）放得進盒子為止，最小 3px。
 * 字寬以 0.6em、列高以 1.2em 估算（等寬字型的一般比例）。
 */
export const MIN_READABLE_FONT = 12
export const MAX_FONT = 13
export const MIN_FIT_FONT = 3

export function fontSizeFor(
  box: { width: number; height: number },
  grid: { cols: number; rows: number },
  fitScreen: boolean,
): number {
  if (grid.cols <= 0 || grid.rows <= 0 || box.width <= 0 || box.height <= 0) return MAX_FONT
  const fitSize = Math.min(box.width / (grid.cols * 0.6), box.height / (grid.rows * 1.2))
  if (!fitScreen) return Math.max(MIN_READABLE_FONT, Math.min(MAX_FONT, Math.floor(fitSize * 2) / 2))
  return Math.max(MIN_FIT_FONT, Math.min(MAX_FONT, Math.floor(fitSize * 2) / 2))
}
