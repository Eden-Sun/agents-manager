export function shownChildren(el: Element): HTMLElement[] {
  return [...el.children].filter((c): c is HTMLElement => c instanceof HTMLElement && c.getClientRects().length > 0)
}

export function measureQuotaAvail(head: HTMLElement, strip: HTMLElement): number {
  const cs = getComputedStyle(head)
  const kids = shownChildren(head)
  let used = (parseFloat(cs.paddingLeft) || 0) + (parseFloat(cs.paddingRight) || 0)
    + (parseFloat(cs.columnGap) || 0) * Math.max(0, kids.length - 1)
  for (const k of kids) {
    if (k !== strip) used += k.getBoundingClientRect().width
  }
  return Math.max(0, head.clientWidth - used)
}

export interface QuotaRowSize {
  width: number
  cellMin: number
}

export function quotaRowSize(widths: number[], fixed: number, gap: number, count: number): QuotaRowSize {
  const cellMin = Math.max(0, ...widths)
  return { cellMin, width: cellMin * widths.length + fixed + gap * Math.max(0, count - 1) }
}

export function measureQuotaRow(row: HTMLElement): QuotaRowSize {
  const kids = shownChildren(row)
  const saved = kids.map(k => k.getAttribute('style'))
  try {
    for (const k of kids) {
      k.style.flex = '0 0 auto'
      k.style.minWidth = 'max-content'
    }
    const widths: number[] = []
    const cs = getComputedStyle(row)
    let fixed = (parseFloat(cs.paddingLeft) || 0) + (parseFloat(cs.paddingRight) || 0)
    for (const k of kids) {
      const style = getComputedStyle(k)
      const width = k.getBoundingClientRect().width
      fixed += (parseFloat(style.marginLeft) || 0) + (parseFloat(style.marginRight) || 0)
      if (k.classList.contains('quota-hp') && !k.classList.contains('off')) widths.push(width)
      else fixed += width
    }
    return quotaRowSize(widths, fixed, parseFloat(cs.columnGap) || 0, kids.length)
  } finally {
    kids.forEach((k, i) => {
      if (saved[i] === null) k.removeAttribute('style')
      else k.setAttribute('style', saved[i]!)
    })
  }
}
