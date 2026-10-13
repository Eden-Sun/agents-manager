import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useMenuKeys } from '../hooks/useMenuKeys'
import { MoreIcon } from './Icons'
import './headMoreMenu.css'

/** 標題列／側欄 project 列的 `⋯`：刪除收進來，照 UI-DECISIONS「一般畫面最多一個常駐危險操作」。 */
export function HeadMoreMenu({
  children,
  label,
  trigger,
  triggerClassName,
}: {
  children: ReactNode
  label: string
  /** 觸發鈕的內容（預設 `⋯`）；專案「＋」等沿用同一套選單、鍵盤與定位時用。 */
  trigger?: ReactNode
  triggerClassName?: string
}) {
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
    delete el.dataset.up
    const { left, right } = el.getBoundingClientRect()
    const edge = 8
    const shift = right > window.innerWidth - edge ? window.innerWidth - edge - right : left < edge ? edge - left : 0
    if (shift !== 0) el.style.setProperty('translate', `${Math.round(shift)}px 0`)
    const clip = el.closest<HTMLElement>('.sidebar-scroll')
    const clipRect = clip?.getBoundingClientRect()
    const floor = Math.min(window.innerHeight, clipRect?.bottom ?? Infinity) - 8
    const ceil = (clipRect?.top ?? 0) + 8
    const r = el.getBoundingClientRect()
    const btnTop = wrap.current?.getBoundingClientRect().top ?? r.top
    // 往下放不下、往上放得下才翻；上下都放不下就維持往下。
    if (r.bottom > floor && btnTop - 6 - r.height >= ceil) el.dataset.up = ''
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
        className={`${triggerClassName ?? 'icon-btn head-menu-btn icon-tip'}${open ? ' on' : ''}`}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={label}
        data-tip={open ? undefined : label}
        onClick={() => setOpen((v) => !v)}
      >
        {trigger ?? <MoreIcon />}
      </button>
      {open ? (
        <div ref={pop} className="head-menu-pop" role="menu" aria-label={label} onClick={() => setOpen(false)} onKeyDown={menuKeys}>
          {children}
        </div>
      ) : null}
    </div>
  )
}
