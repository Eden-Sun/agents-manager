import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { MoreIcon } from './Icons'

/**
 * 標題列的 `⋯`：收「不常按、又不該常駐在標題列上」的動作。
 *
 * 起因是「中止」與「刪除」兩顆紅框按鈕肩並肩——紅色因此變成標題列的常態色，而且要停掉
 * 一個 team 時很容易多按一格就把它整筆刪掉。UI-DECISIONS 已經定了「一般畫面最多一個常駐
 * 危險操作」，所以中止／清理留在外面，刪除收進來。
 *
 * 側欄的 project 標題列（新增 Bot 的 `＋` 旁邊就是刪除專案的 `✕`）用的是同一顆。
 */
export function HeadMoreMenu({ children, label }: { children: ReactNode; label: string }) {
  const [open, setOpen] = useState(false)
  const wrap = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (wrap.current && !wrap.current.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDoc)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  return (
    <div className="head-menu" ref={wrap}>
      <button
        type="button"
        className={`icon-btn head-menu-btn icon-tip${open ? ' on' : ''}`}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={label}
        title={label}
        data-tip={open ? undefined : label}
        onClick={() => setOpen((v) => !v)}
      >
        <MoreIcon />
      </button>
      {open ? (
        // 點到裡面任何一顆按鈕就關起來：每一項都是「開確認框」或「離開」，沒有留著的理由。
        <div className="head-menu-pop" role="menu" onClick={() => setOpen(false)}>
          {children}
        </div>
      ) : null}
    </div>
  )
}
