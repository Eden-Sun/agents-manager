/**
 * 渲染計數點：測試把 `globalThis.__amRenderProbe` 設成計數函式，就能數某個元件實際渲染了幾次（量測用，
 * 例如側欄一次狀態變化重算幾列）。正式環境沒有那個全域，這是一個空呼叫。
 */
export function renderProbe(name: string): void {
  const probe = (globalThis as { __amRenderProbe?: (n: string) => void }).__amRenderProbe
  if (probe) probe(name)
}
