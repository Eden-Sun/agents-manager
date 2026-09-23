/**
 * 把文字放進剪貼簿，回傳成不成功。**同步**先走 `execCommand('copy')`：它只認使用者手勢、
 * 不跳權限提示、http 的 LAN 位址也能用；`navigator.clipboard.writeText` 在非 secure context
 * 是 undefined、在某些 Chrome 情境會停在權限提示上不 resolve（2026-09-08 實測），所以只當
 * 備援，而且呼叫端不要等它。
 *
 * 用法：`copyText(s).then(ok => …)`；同步路徑成功時 promise 立刻 resolve(true)。
 */
export function copyText(text: string): Promise<boolean> {
  let copied = false
  try {
    copied = legacyCopy(text)
  } catch {
    copied = false
  }
  if (copied) return Promise.resolve(true)
  try {
    if (!navigator.clipboard) return Promise.resolve(false)
    return Promise.resolve(navigator.clipboard.writeText(text)).then(
      () => true,
      () => false,
    )
  } catch {
    return Promise.resolve(false)
  }
}

function legacyCopy(text: string): boolean {
  let ok = false
  let ta: HTMLTextAreaElement | null = null
  let prev: HTMLElement | null = null
  try {
    ta = document.createElement('textarea')
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
    prev = document.activeElement as HTMLElement | null
    ta.focus({ preventScroll: true })
    ta.select()
    ta.setSelectionRange(0, text.length)
    ok = document.execCommand('copy')
  } catch {
    ok = false
  } finally {
    if (ta?.parentNode) {
      try {
        ta.parentNode.removeChild(ta)
      } catch {
        try {
          ta.remove()
        } catch {
          // Best effort: the DOM may no longer contain the temporary textarea.
        }
        ok = false
      }
    }
    try {
      window.getSelection()?.removeAllRanges()
    } catch {
      ok = false
    }
    try {
      prev?.focus?.({ preventScroll: true })
    } catch {
      ok = false
    }
  }
  return ok
}
