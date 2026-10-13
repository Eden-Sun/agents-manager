/**
 * 點主力（★）晶片 → 側欄把那顆 bot 的列捲到可視區頂端（#1676，2026-10-13 使用者：「點了主力的 bot 後，
 * menu 要 scroll 到該 bot 置頂的位置」）。只有主力晶片這個入口；其他入口（鍵盤↑↓、搜尋、通知）仍是原本的
 * `nearest`（選取換了才捲進畫面）。
 *
 * 直接捲 `.sidebar-scroll` 而不是 `scrollIntoView`：手機側欄是抽屜，點晶片時它是關著的（離畫面的
 * `translate`），`scrollIntoView` 會連帶捲祖先、推走 `.app`；只動自己的捲動容器，抽屜之後打開就已經在位。
 */
export const BOT_TOP_EVENT = 'am:scroll-bot-top'

let pending: string | null = null

/** 有這顆 bot 的置頂請求還沒做完：Sidebar 選取換了的 `nearest` 捲動要讓路，不然兩個捲動搶。 */
export function botTopPending(botId: string | null): boolean {
  return botId !== null && pending === botId
}

export function requestBotToTop(botId: string): void {
  if (typeof window === 'undefined') return
  pending = botId
  window.dispatchEvent(new CustomEvent(BOT_TOP_EVENT, { detail: { botId } }))
}

/** 請求處理完（找到列捲了、或找不到）就清掉，只清自己那一筆。 */
export function finishBotToTop(botId: string): void {
  if (pending === botId) pending = null
}

/** 把 bot 列頂端貼齊 `.sidebar-scroll` 頂端；列不在 DOM（被搜尋濾掉）就什麼都不做。 */
export function scrollBotRowToTop(botId: string): void {
  if (typeof document === 'undefined') return
  const row = document.querySelector<HTMLElement>(`.sidebar-scroll .bot-row[data-bot-id="${CSS.escape(botId)}"]`)
  const box = row?.closest<HTMLElement>('.sidebar-scroll')
  if (!row || !box) return
  const top = Math.max(0, box.scrollTop + row.getBoundingClientRect().top - box.getBoundingClientRect().top)
  if (typeof box.scrollTo === 'function') box.scrollTo({ top, behavior: 'smooth' })
  else box.scrollTop = top
}
