import { useState } from 'react'

/**
 * 「標籤 + 值 + 點一下複製」的小晶片。
 *
 * 原本只長在 `TeamPanel` 的副標題列（整合分支 / worktree），現在 `ChatPanel` 的 run 識別列
 * （pane / agent / run id）也要同一套外觀與手感，所以抽出來共用。CSS 沿用既有的
 * `.team-copy*`，沒有新增第二套樣式。
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
      className={`team-copy${done ? ' done' : ''}${className ? ' ' + className : ''}`}
      title={`${title}\n${value}（點擊複製）`}
      onClick={() => {
        void navigator.clipboard
          ?.writeText(value)
          .then(() => {
            setDone(true)
            setTimeout(() => setDone(false), 1200)
          })
          .catch(() => undefined)
      }}
    >
      <span className="team-copy-label">{label}</span>
      <span className="team-copy-value mono">{value}</span>
      {done ? <span className="team-copy-ok">已複製</span> : null}
    </button>
  )
}
