import { useState } from 'react'
import { useStore } from '../store/store'
import { copyText } from '../lib/copyText'

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
        void copyText(url).then((done) => {
          if (done) ok()
          else notify('error', '複製失敗，請手動選取網址')
        })
      }}
    >
      {text}
      {done ? <span className="term-link-ok">已複製</span> : null}
    </button>
  )
}
