import { copyText } from '../lib/copyText'
import { useState } from 'react'

/**
 * 「標籤 + 值 + 點一下複製」的小晶片。
 *
 * 值是空字串就整個不渲染——沒有 pane id 的 run 不該留一顆空晶片。
 */
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
  const [done, setDone] = useState(false)
  if (!value) return null
  return (
    <button
      type="button"
      className={`copy-chip${done ? ' done' : ''}${className ? ' ' + className : ''}`}
      title={`${title}\n${value}（點擊複製）`}
      onClick={() => {
        void copyText(value).then((ok) => {
          if (!ok) return
          setDone(true)
          setTimeout(() => setDone(false), 1200)
        })
      }}
    >
      <span className="copy-chip-label">{label}</span>
      <span className="copy-chip-value mono">{value}</span>
      {done ? <span className="copy-chip-ok">已複製</span> : null}
    </button>
  )
}
