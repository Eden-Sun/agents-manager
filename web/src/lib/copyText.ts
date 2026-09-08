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
  ta.setAttribute('readonly', '')
  ta.style.position = 'fixed'
  ta.style.opacity = '0'
  ta.style.top = '0'
  ta.style.left = '0'
  document.body.appendChild(ta)
  // 記住原本的焦點：copy 完要還回去，不然輸入框的游標會不見。
  const prev = document.activeElement as HTMLElement | null
  ta.focus()
  ta.select()
  let ok = false
  try {
    ok = document.execCommand('copy')
  } catch {
    ok = false
  }
  document.body.removeChild(ta)
  prev?.focus?.()
  return ok
}
