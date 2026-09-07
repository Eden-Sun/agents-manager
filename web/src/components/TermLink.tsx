import { useState } from 'react'

/** 終端畫面裡的一條 URL：點一下複製。拆成獨立檔是為了 fast refresh（檔案只 export 元件）。 */
export function TermLink({ url, text }: { url: string; text: string }) {
  const [done, setDone] = useState(false)
  return (
    <button
      type="button"
      className={`term-link${done ? ' done' : ''}`}
      title={`點一下複製整條網址\n${url}`}
      onClick={() => {
        void navigator.clipboard
          ?.writeText(url)
          .then(() => {
            setDone(true)
            setTimeout(() => setDone(false), 1200)
          })
          .catch(() => undefined)
      }}
    >
      {text}
      {done ? <span className="term-link-ok">已複製</span> : null}
    </button>
  )
}
