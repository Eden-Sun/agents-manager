export const BOT_TOP_EVENT = 'am:scroll-bot-top'

let pending: string | null = null

export function botTopPending(botId: string | null): boolean {
  return botId !== null && pending === botId
}

export function requestBotToTop(botId: string): void {
  if (typeof window === 'undefined') return
  pending = botId
  window.dispatchEvent(new CustomEvent(BOT_TOP_EVENT, { detail: { botId } }))
}

export function finishBotToTop(botId: string): void {
  if (pending === botId) pending = null
}

export function scrollBotRowToTop(botId: string): void {
  if (typeof document === 'undefined') return
  const row = document.querySelector<HTMLElement>(`.sidebar-scroll .bot-row[data-bot-id="${CSS.escape(botId)}"]`)
  const box = row?.closest<HTMLElement>('.sidebar-scroll')
  if (!row || !box) return
  const top = Math.max(0, box.scrollTop + row.getBoundingClientRect().top - box.getBoundingClientRect().top)
  if (typeof box.scrollTo === 'function') box.scrollTo({ top, behavior: 'smooth' })
  else box.scrollTop = top
}
