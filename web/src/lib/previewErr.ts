/** 預覽面板顯示哪一條錯誤：按鈕（啟動／停止）失敗的優先；背景讀狀態失敗的那條在下一次讀成功時由呼叫端清掉。 */
export function shownPreviewErr<T>(start: T | null, load: T | null): T | null {
  return start ?? load
}
