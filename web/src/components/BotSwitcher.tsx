import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { botLamp, useStore } from '../store/store'
import { KindTag } from './KindTag'
import { StatusLamp } from './StatusLamp'

/**
 * 手機標題列的 bot 名：點一下是**換 bot**，不是改名（2026-09-09 使用者決定）。桌面有側欄，
 * 名字點了直接改；手機側欄收在抽屜裡，換 bot 要開抽屜、找、點、再等抽屜收——太遠。
 * 這裡下拉列出所有**母 bot**（`parent_bot_id` 為 null、沒刪掉），依 project 分組，點了就切。
 * 改名在手機走 Bot 設定的名稱欄。
 */
export function BotSwitcher({ botId, name }: { botId: string; name: string }) {
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLDivElement>(null)
  const popRef = useRef<HTMLDivElement>(null)
  // `.main-title-row` 會把超出的東西剪掉（標題列忙時裁尾巴），絕對定位在裡面的泡泡整個看不到
  // （2026-09-09 手機實測：點了沒東西可選）。所以泡泡用 portal 掛到 body、fixed 定位在按鈕底下。
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null)
  useLayoutEffect(() => {
    if (!open) return
    const b = ref.current?.getBoundingClientRect()
    if (b) setPos({ top: Math.round(b.bottom + 6), left: Math.max(8, Math.round(b.left)) })
  }, [open])
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

  // 母 bot：不是子 agent、也不是 team 成員（pm / dev / rev 有自己的 Team 面板）。
  const mothers = bots.filter((b) => !b.parent_bot_id && !b.team_id)
  const groups = projects
    .map((p) => ({ project: p, bots: mothers.filter((b) => b.project_id === p.id) }))
    .filter((g) => g.bots.length > 0)

  return (
    <div className="bot-switcher" ref={ref}>
      <button
        type="button"
        className="bot-name-btn bot-switcher-btn"
        aria-haspopup="listbox"
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
            <div ref={popRef} className="bot-switcher-pop" role="listbox" aria-label="切換 bot" style={{ top: pos.top, left: pos.left }}>
              {groups.map((g) => (
                <div key={g.project.id} className="bot-switcher-group">
                  <div className="bot-switcher-project">⌗ {g.project.label}</div>
                  {g.bots.map((b) => (
                    <button
                      key={b.id}
                      type="button"
                      role="option"
                      aria-selected={b.id === botId}
                      className={`bot-switcher-item${b.id === botId ? ' on' : ''}`}
                      onClick={() => {
                        setOpen(false)
                        if (b.id !== botId) selectBot(b.id)
                      }}
                    >
                      <RowLamp botId={b.id} />
                      <KindTag kind={b.kind} />
                      <span className="bot-switcher-name">{b.name}</span>
                    </button>
                  ))}
                </div>
              ))}
            </div>,
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
