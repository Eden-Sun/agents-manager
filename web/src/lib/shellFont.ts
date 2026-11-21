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
