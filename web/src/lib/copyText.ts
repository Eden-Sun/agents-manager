export function copyText(text: string): Promise<boolean> {
  if (legacyCopy(text)) return Promise.resolve(true)
  if (!navigator.clipboard) return Promise.resolve(false)
  return navigator.clipboard.writeText(text).then(
    () => true,
    () => false,
  )
}

function legacyCopy(text: string): boolean {
  const ta = document.createElement('textarea')
  ta.value = text
  ta.setAttribute('inputmode', 'none')
  ta.style.position = 'fixed'
  ta.style.opacity = '0'
  ta.style.top = '0'
  ta.style.left = '0'
  ta.style.fontSize = '16px'
  document.body.appendChild(ta)
  const prev = document.activeElement as HTMLElement | null
  ta.focus({ preventScroll: true })
  ta.select()
  ta.setSelectionRange(0, text.length)
  let ok = false
  try {
    ok = document.execCommand('copy')
  } catch {
    ok = false
  }
  document.body.removeChild(ta)
  window.getSelection()?.removeAllRanges()
  prev?.focus?.({ preventScroll: true })
  return ok
}

export function asyncCopyAvailable(): boolean {
  return typeof window !== 'undefined' && window.isSecureContext === true && Boolean(navigator.clipboard)
}
