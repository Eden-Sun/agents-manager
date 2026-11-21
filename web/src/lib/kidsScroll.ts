const SEL = '.bot-row.compact > .bot-main'

export function syncKidsScroll(e: { currentTarget: HTMLElement }) {
  const me = e.currentTarget
  const kids = me.closest('.bot-kids')
  if (!kids) return
  const x = me.scrollLeft
  for (const el of kids.querySelectorAll<HTMLElement>(SEL)) {
    if (el !== me && Math.abs(el.scrollLeft - x) > 0.5) el.scrollLeft = x
  }
}

export function wheelKidsScroll(e: { currentTarget: HTMLElement; deltaX: number; deltaY: number; shiftKey: boolean; preventDefault: () => void }) {
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
  for (const el of rows) el.scrollLeft = next
}
