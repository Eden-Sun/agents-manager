import { useState } from 'react'
import { useStore } from '../store/store'

/**
 * `navigator.clipboard` 只在 secure context 存在——LAN IP、非 localhost 的 http 來源下是
 * `undefined`（使用者實際回報「點了沒複製到」多半是這個情境）。退回舊式做法：塞一個看不見的
 * textarea、選取、`document.execCommand('copy')`。兩條路都失敗才算真的失敗。
 */
function legacyCopy(text: string): boolean {
  const ta = document.createElement('textarea')
  ta.value = text
  ta.style.position = 'fixed'
  ta.style.opacity = '0'
  ta.style.top = '0'
  ta.style.left = '0'
  document.body.appendChild(ta)
  ta.focus()
  ta.select()
  let ok = false
  try {
    ok = document.execCommand('copy')
  } catch {
    ok = false
  }
  document.body.removeChild(ta)
  return ok
}

/** 終端畫面裡的一條 URL：點一下複製。拆成獨立檔是為了 fast refresh（檔案只 export 元件）。 */
export function TermLink({ url, text }: { url: string; text: string }) {
  const [done, setDone] = useState(false)
  const notify = useStore((s) => s.notify)
  return (
    <button
      type="button"
      className={`term-link${done ? ' done' : ''}`}
      title={`點一下複製整條網址\n${url}`}
      onClick={() => {
        const ok = () => {
          setDone(true)
          setTimeout(() => setDone(false), 1200)
        }
        const fail = () => notify('error', '複製失敗，請手動選取網址')
        const fallback = () => {
          if (legacyCopy(url)) ok()
          else fail()
        }
        if (navigator.clipboard) navigator.clipboard.writeText(url).then(ok, fallback)
        else fallback()
      }}
    >
      {text}
      {done ? <span className="term-link-ok">已複製</span> : null}
    </button>
  )
}
