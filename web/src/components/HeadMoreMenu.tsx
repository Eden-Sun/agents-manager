import { useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useMenuKeys } from '../hooks/useMenuKeys'
import { useDismiss } from '../lib/useDismiss'
import { MoreIcon } from './Icons'
import './headMoreMenu.css'

export function HeadMoreMenu({
  children,
  label,
  trigger,
  triggerClassName,
}: {
  children: ReactNode
  label: string
  trigger?: ReactNode
  triggerClassName?: string
}) {
  const [open, setOpen] = useState(false)
  const wrap = useRef<HTMLDivElement>(null)
  const pop = useRef<HTMLDivElement>(null)
  const btn = useRef<HTMLButtonElement>(null)
  const menuKeys = useMenuKeys(open, pop, btn, () => setOpen(false))

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
    if (r.bottom > floor && btnTop - 6 - r.height >= ceil) el.dataset.up = ''
  }, [open])

  useDismiss(open, wrap, () => setOpen(false))

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
