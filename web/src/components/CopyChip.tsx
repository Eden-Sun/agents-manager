import { copyText } from '../lib/copyText'
import { useState } from 'react'
import './copyChip.css'

/** 「標籤 + 值 + 點一下複製」的小晶片；值是空字串就不渲染（沒有 pane id 的 run 不留空晶片）。 */
export function CopyChip({
  label,
  value,
  title,
  className,
}: {
  label: string
  value: string
  title: string
  /** 額外的 class（目前只有 `desk-only`：手機的副標題列放不下這麼多晶片）。 */
  className?: string
}) {
  const [done, setDone] = useState<'ok' | 'fail' | null>(null)
  if (!value) return null
  return (
    <button
      type="button"
      className={`copy-chip${done === 'ok' ? ' done' : ''}${className ? ' ' + className : ''}`}
      title={`${title}\n${value}（點擊複製）`}
      onClick={() => {
        void copyText(value).then((ok) => {
          // 失敗也要有回饋：以前靜默 return，手機上看起來就是「點了沒反應」。
          setDone(ok ? 'ok' : 'fail')
          setTimeout(() => setDone(null), ok ? 1200 : 2400)
        })
      }}
    >
      <span className="copy-chip-label">{label}</span>
      <span className="copy-chip-value mono">{value}</span>
      {done === 'ok' ? <span className="copy-chip-ok">已複製</span> : null}
      {done === 'fail' ? <span className="copy-chip-fail">複製失敗，長按選取</span> : null}
    </button>
  )
}
