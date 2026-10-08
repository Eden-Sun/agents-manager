/**
 * 桌機 Bot 設定卡的「未儲存守門」登記表（#925）。
 *
 * 設定卡是非模態的（沒有 `.modal-backdrop`／`aria-modal`），所以 `dialogOpen()` 看不到它；點外面與 Esc 走卡片自己的
 * `requestClose()`，但鍵盤導覽（⌥↑／⌥↓、Control+1…9、側欄列的 ↑／↓）直接換 bot／專案，變更就無聲丟掉。
 * 這裡讓卡片登記一個守門函式，鍵盤路徑換人之前先問它。`dialogOpen()` 的語意不變：沒有變更時快捷鍵照常能用。
 */
type Guard = () => boolean

let current: Guard | null = null

/**
 * 登記守門函式。`fn` 回 true ＝ 有未儲存變更、已經把確認框打開了，呼叫端**不要**離開；false ＝ 可以走。
 * 同時只有一個設定卡，重複登記以最後一個為準。回傳的解除函式只解除自己登記的那一個（舊卡卸載不會拆掉新卡的）。
 */
export function registerSettingsLeaveGuard(fn: Guard): () => void {
  current = fn
  return () => {
    if (current === fn) current = null
  }
}

/** 有未儲存變更而擋下離開了嗎？沒登記＝false。守門函式會順手把確認框打開，所以只在「要離開」的那一刻呼叫。 */
export function settingsBlocksLeave(): boolean {
  return current ? current() : false
}
