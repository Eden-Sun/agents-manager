/**
 * 只採最新一次請求的結果：`begin()` 領一張票，`isCurrent(ticket)` 為 false 就是有更新的請求在後面了，
 * 這次的結果（成功、失敗、收尾）都不該寫進畫面。
 */
export function createLatestOnly() {
  let seq = 0
  return {
    begin: () => ++seq,
    isCurrent: (ticket: number) => ticket === seq,
  }
}
