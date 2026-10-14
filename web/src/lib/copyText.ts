/**
 * 把文字放進剪貼簿，回傳成不成功。**同步**先走 `execCommand('copy')`：它只認使用者手勢、
 * 不跳權限提示、http 的 LAN 位址也能用；`navigator.clipboard.writeText` 在非 secure context
 * 是 undefined、在某些 Chrome 情境會停在權限提示上不 resolve（2026-09-08 實測），所以只當
 * 備援，而且呼叫端不要等它。
 *
 * 用法：`copyText(s).then(ok => …)`；同步路徑成功時 promise 立刻 resolve(true)。
 */
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
  // iOS Safari：readonly 的 textarea `select()` 選不到字，execCommand 會回 false、什麼都沒複製
  // （手機點 pane 名沒反應的原因）。改用 inputmode=none 擋鍵盤，並用 setSelectionRange 選取。
  ta.setAttribute('inputmode', 'none')
  ta.style.position = 'fixed'
  ta.style.opacity = '0'
  ta.style.top = '0'
  ta.style.left = '0'
  // 小於 16px 的輸入框 focus 時 iOS 會自動放大畫面。
  ta.style.fontSize = '16px'
  document.body.appendChild(ta)
  // 記住原本的焦點：copy 完要還回去，不然輸入框的游標會不見。
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

/**
 * 等過網路才複製（先抓連結、再寫剪貼簿）做不做得到。非 secure context（手機用 http 開 LAN 位址）沒有
 * `navigator.clipboard`，而 `execCommand('copy')` 只在同一個點擊手勢裡有效：await 之後一定失敗。
 * 做不到時不要給「複製」鈕讓使用者按了才失敗，直接給可選取的文字（2026-10-14 使用者）。
 */
export function asyncCopyAvailable(): boolean {
  return typeof window !== 'undefined' && window.isSecureContext === true && Boolean(navigator.clipboard)
}
