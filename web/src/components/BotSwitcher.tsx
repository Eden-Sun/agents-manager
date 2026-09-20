import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { CSSProperties, KeyboardEvent as ReactKeyboardEvent, RefObject } from 'react'
import { useMenuKeys } from '../hooks/useMenuKeys'
import type { Bot, Project } from '../api/types'
import { botLamp, useStore } from '../store/store'
import { KindTag } from './KindTag'
import { StatusLamp } from './StatusLamp'
import './botSwitcher.css'

/**
 * 手機標題列的 bot 名：點一下是換 bot 而非改名（2026-09-09 使用者決定）；列出母 bot，依 project 分組。
 * 手機改名走 Bot 設定的名稱欄。
 */
export function BotSwitcher({ botId, name }: { botId?: string; name: string }) {
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLDivElement>(null)
  const btnRef = useRef<HTMLButtonElement>(null)
  const popRef = useRef<HTMLDivElement>(null)
  // `.main-title-row` 會裁掉溢出，泡泡在裡面整個看不到（2026-09-09 手機實測）→ portal 到 body、fixed 定位。
  const [pos, setPos] = useState<{ top: number; left: number; maxWidth: number } | null>(null)
  useLayoutEffect(() => {
    if (!open) return
    const b = ref.current?.getBoundingClientRect()
    if (!b) return
    const left = Math.max(8, Math.round(b.left))
    // 從按鈕左緣長出，`max-width: 100vw` 擋不住右邊界，長名字會出畫面。
    setPos({ top: Math.round(b.bottom + 6), left, maxWidth: Math.max(200, window.innerWidth - left - 8) })
  }, [open])
  // 方向鍵／Home／End 移動、Esc／Tab 關掉並把焦點還給按鈕（同 HeadMoreMenu）。
  const menuKeys = useMenuKeys(open, popRef, btnRef, () => setOpen(false))
  const selectBot = useStore((s) => s.selectBot)
  const projects = useStore((s) => s.projects)
  const bots = useStore((s) => s.bots)

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent | TouchEvent) => {
      const t = e.target as Node
      if (ref.current?.contains(t) || popRef.current?.contains(t)) return
      setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    document.addEventListener('touchstart', onDoc)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDoc)
      document.removeEventListener('touchstart', onDoc)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  const mothers = bots.filter((b) => !b.parent_bot_id)
  const groups = projects
    .map((p) => ({ project: p, bots: mothers.filter((b) => b.project_id === p.id) }))
    .filter((g) => g.bots.length > 0)

  return (
    <div className="bot-switcher" ref={ref}>
      <button
        ref={btnRef}
        type="button"
        className="bot-name-btn bot-switcher-btn"
        aria-haspopup="menu"
        aria-expanded={open}
        title="換一顆 bot"
        onClick={() => setOpen((v) => !v)}
      >
        <strong>{name}</strong>
        <span className="bot-switcher-chev" aria-hidden="true">
          ▾
        </span>
      </button>
      {open && pos
        ? createPortal(
            <BotSwitcherMenu
              popRef={popRef}
              style={{ top: pos.top, left: pos.left, maxWidth: pos.maxWidth }}
              groups={groups}
              botId={botId}
              onKeyDown={menuKeys}
              onPick={(id) => {
                setOpen(false)
                if (id !== botId) selectBot(id)
              }}
            />,
            document.body,
          )
        : null}
    </div>
  )
}

/** 每列自己訂閱自己的燈：selector 回傳字串才穩定，回傳整張 map 會讓 zustand 每次 render 都換新物件而無限重繪。 */
function RowLamp({ botId }: { botId: string }) {
  const lamp = useStore((s) => botLamp(s, botId))
  return <StatusLamp lamp={lamp} />
}

export function BotSwitcherMenu({
  popRef,
  style,
  groups,
  botId,
  onPick,
  onKeyDown,
}: {
  popRef?: RefObject<HTMLDivElement | null>
  style?: CSSProperties
  groups: { project: Project; bots: Bot[] }[]
  botId?: string
  onPick: (id: string) => void
  onKeyDown?: (e: ReactKeyboardEvent<HTMLElement>) => void
}) {
  return (
    <div ref={popRef} className="bot-switcher-pop" role="menu" aria-label="切換 bot" style={style} onKeyDown={onKeyDown}>
      {groups.map((g) => (
        <div key={g.project.id} className="bot-switcher-group" role="group" aria-label={g.project.label}>
          <div className="bot-switcher-project" aria-hidden="true">⌗ {g.project.label}</div>
          {g.bots.map((b) => (
            <button
              key={b.id}
              type="button"
              role="menuitemradio"
              aria-checked={b.id === botId}
              tabIndex={-1}
              className={`bot-switcher-item${b.id === botId ? ' on' : ''}`}
              onClick={() => onPick(b.id)}
            >
              <RowLamp botId={b.id} />
              <KindTag kind={b.kind} />
              <span className="bot-switcher-name">{b.name}</span>
            </button>
          ))}
        </div>
      ))}
    </div>
  )
}
