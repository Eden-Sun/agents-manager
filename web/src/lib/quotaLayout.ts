/** 標題列額度區：一格（身分名＋5h／7d 條＋兩個百分比）不疊字所需的寬度。實測 cc0 一格內容約 114px＋格線。 */
export const QUOTA_CELL_MIN = 116

/** 標題列裡額度以外要留的寬度：★／⚙／分頁鈕／名字下限（名字與分頁的收縮優先序在額度之前）。1440 實測約 340。 */
export const QUOTA_HEAD_RESERVED = 340

/** 舊規則的底線：窄到這個寬度不論幾格都收成單窗口。 */
export const QUOTA_COLLAPSE_FLOOR = 604

/**
 * 額度區要不要收成單窗口。量的是標題列（父節點）寬，不量 `.quota-strip` 自己：它跟名字一起 flex 分剩餘寬度，
 * 收合前後寬度會互相牽動而來回震盪。門檻跟著格數走：格數多就要更寬才放得下完整量表。
 */
export function quotaCollapsed(headWidth: number, cells: number): boolean {
  return headWidth < Math.max(QUOTA_COLLAPSE_FLOOR, QUOTA_HEAD_RESERVED + cells * QUOTA_CELL_MIN)
}
