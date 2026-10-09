/**
 * 瀏覽器「上一頁」（popstate）該做什麼（#935／#936）。純函式，`store/routeSync.ts` 的 `onPop` 照結果動作。
 *
 * 優先順序（上層先吃）：
 * 1. 有對話框開著 → `close-dialog`：上一頁只關最上層的對話框（把借來的歷史格放回去，再對它派 Esc），不關抽屜、不離開畫面。
 * 2. 手機抽屜開著 → `close-drawer`：只關抽屜（那格 URL 沒變）。
 * 3. 正要離開 bot 設定、而且有未儲存變更 → `stay`：留在設定，把歷史格放回去（確認框已由守門打開）。
 * 4. 其餘 → `navigate`：照網址套用新路由。
 *
 * `guardBlocks` 是函式，不是布林：`settingsBlocksLeave()` 有副作用（會打開「有未儲存的變更」確認框），
 * 只有走到第 3 步（真的要離開設定）才呼叫，而且最多一次。
 */
export type PopDecision = 'close-dialog' | 'close-drawer' | 'stay' | 'navigate'

export function popDecision(input: {
  drawerOpen: boolean
  dialogOpen: boolean
  /** `lastRoute` 是某顆 bot 的設定，而新路由不是同一顆的設定。 */
  leavingSettings: boolean
  guardBlocks: () => boolean
}): PopDecision {
  if (input.dialogOpen) return 'close-dialog'
  if (input.drawerOpen) return 'close-drawer'
  if (input.leavingSettings && input.guardBlocks()) return 'stay'
  return 'navigate'
}
