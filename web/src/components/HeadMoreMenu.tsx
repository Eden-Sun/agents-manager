import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useMenuKeys } from '../hooks/useMenuKeys'
import { MoreIcon } from './Icons'
import './headMoreMenu.css'

/** 標題列／側欄 project 列的 `⋯`：刪除收進來，照 UI-DECISIONS「一般畫面最多一個常駐危險操作」。 */
export function HeadMoreMenu({ children, label }: { children: ReactNode; label: string }) {
  const [open, setOpen] = useState(false)
  const wrap = useRef<HTMLDivElement>(null)
  const pop = useRef<HTMLDivElement>(null)
  const btn = useRef<HTMLButtonElement>(null)
  // role="menu" 的鍵盤行為（同 Tools／ModelPicker）。
  const menuKeys = useMenuKeys(open, pop, btn, () => setOpen(false))

  // CSS 只能選一邊展開，`⋯` 在最右時選單滑出右緣（390px 實測 right=510）；打開時量一次推回來。
  useLayoutEffect(() => {
    const el = pop.current
    if (!open || !el) return
    el.style.removeProperty('translate')
    const { left, right } = el.getBoundingClientRect()
    const edge = 8
    const shift = right > window.innerWidth - edge ? window.innerWidth - edge - right : left < edge ? edge - left : 0
    if (shift !== 0) el.style.setProperty('translate', `${Math.round(shift)}px 0`)
  }, [open])

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
        ref={btn}
        type="button"
        className={`icon-btn head-menu-btn icon-tip${open ? ' on' : ''}`}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={label}
        data-tip={open ? undefined : label}
        onClick={() => setOpen((v) => !v)}
      >
        <MoreIcon />
      </button>
      {open ? (
        <div ref={pop} className="head-menu-pop" role="menu" aria-label={label} onClick={() => setOpen(false)} onKeyDown={menuKeys}>
          {children}
        </div>
      ) : null}
    </div>
  )
}
