/**
 * 子 agent 列的左右滑要「整組一起」（2026-09-10 使用者）：每列自己是一個捲動區（燈號、樹線、
 * 選單鈕才能固定不動），但捲動量在同一個 `.bot-kids` 裡同步。
 *
 * - 手指／捲軸捲某一列 → `syncKidsScroll` 把同組其他列推到同一個 scrollLeft。
 * - 滾輪橫捲在容器上攔下來自己分配（`wheelKidsScroll`）：不然滾在較短的那列會先撞到它的
 *   底、拖著整組一起停，看不完最長那列。
 */
const SEL = '.bot-row.compact > .bot-main'
const pendingProgrammaticScroll = new WeakMap<HTMLElement, number>()

function setScrollLeft(el: HTMLElement, x: number) {
  const max = Math.max(0, el.scrollWidth - el.clientWidth)
  const target = Math.min(max, Math.max(0, x))
  if (Math.abs(el.scrollLeft - target) <= 0.5) return
  // `scroll` is delivered after setting scrollLeft. Remember the clamped destination so its event
  // cannot be mistaken for a user scroll and feed a shorter row's smaller maximum back upstream.
  pendingProgrammaticScroll.set(el, target)
  el.scrollLeft = target
}

export function syncKidsScroll(e: { currentTarget: HTMLElement }) {
  const me = e.currentTarget
  const pending = pendingProgrammaticScroll.get(me)
  if (pending !== undefined) {
    pendingProgrammaticScroll.delete(me)
    if (Math.abs(me.scrollLeft - pending) <= 0.5) return
  }
  const kids = me.closest('.bot-kids')
  if (!kids) return
  const x = me.scrollLeft
  for (const el of kids.querySelectorAll<HTMLElement>(SEL)) {
    if (el !== me) setScrollLeft(el, x)
  }
}

export function wheelKidsScroll(e: { currentTarget: HTMLElement; deltaX: number; deltaY: number; shiftKey: boolean; preventDefault: () => void }) {
  // 只接橫向：shift+滾輪在 mac 會給 deltaX，trackpad 橫滑也是 deltaX。直向留給側欄自己捲。
  const dx = e.deltaX !== 0 ? e.deltaX : e.shiftKey ? e.deltaY : 0
  if (dx === 0) return
  const rows = [...e.currentTarget.querySelectorAll<HTMLElement>(SEL)]
  if (rows.length === 0) return
  const max = Math.max(...rows.map((el) => el.scrollWidth - el.clientWidth))
  if (max <= 0) return
  const cur = Math.max(...rows.map((el) => el.scrollLeft))
  const next = Math.min(max, Math.max(0, cur + dx))
  if (next === cur) return
  e.preventDefault()
  for (const el of rows) setScrollLeft(el, next)
}
