import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useMenuKeys } from '../hooks/useMenuKeys'
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
  const pop = useRef<HTMLDivElement>(null)
  const btn = useRef<HTMLButtonElement>(null)
  // role="menu" 的鍵盤行為（Tools／ModelPicker 用的同一顆）：開啟時焦點進選單、↑↓/Home/End 走項目、
  // Esc／Tab 關掉並把焦點還給 ⋯。以前只有 role 沒有行為，Tab 直接走出去。
  const menuKeys = useMenuKeys(open, pop, btn, () => setOpen(false))

  // 選單是錨在按鈕上的 absolute 方塊，CSS 只能選一邊展開：手機的 team 標題列設成往右展開（`⋯`
  // 換行到最左時才放得下），但平常 `⋯` 在最右邊，整個選單就滑出右緣、裡面的項目點不到
  // （390px 實測 right=510）。打開當下量一次，超出視窗就推回來——按鈕這次落在哪，CSS 不知道。
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
        // 點到裡面任何一顆按鈕就關起來：每一項都是「開確認框」或「離開」，沒有留著的理由。
        <div ref={pop} className="head-menu-pop" role="menu" aria-label={label} onClick={() => setOpen(false)} onKeyDown={menuKeys}>
          {children}
        </div>
      ) : null}
    </div>
  )
}
