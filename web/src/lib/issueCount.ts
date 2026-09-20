/** 清單頁 `load` 的上限；比按鈕上的計數請求（100）小。 */
export const ISSUE_PAGE_LIMIT = 50

/**
 * 清單頁抓回來的 open 筆數能不能拿來更新按鈕上的「N open」。
 * 抓滿一頁（`>= limit`）只代表「至少這麼多」，拿去覆蓋 limit=100 那次的計數會把 73 變成 50。
 */
export function openCountFromPage(prev: number | null, pageLength: number, limit = ISSUE_PAGE_LIMIT): number | null {
  return pageLength >= limit ? prev : pageLength
}
