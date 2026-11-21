import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { CSSProperties, KeyboardEvent as ReactKeyboardEvent, RefObject } from 'react'
import { useMenuKeys } from '../hooks/useMenuKeys'
import type { Bot, Project } from '../api/types'
import { useShallow } from 'zustand/react/shallow'
import { botsOfProject, orderedProjects, useStore } from '../store/store'
import { useBotLamp } from '../hooks/useBotLamp'
import { KindTag } from './KindTag'
import { StatusLamp } from './StatusLamp'
import { ExpertDeputyTag, ExpertRoleTag } from './ExpertBadge'
import { groupExpertSections } from '../lib/expertSections'
import './botSwitcher.css'
import { t, useT } from '../lib/i18n'

export function BotSwitcher({ botId, name }: { botId?: string; name: string }) {
  useT()
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLDivElement>(null)
  const btnRef = useRef<HTMLButtonElement>(null)
  const popRef = useRef<HTMLDivElement>(null)
  const [pos, setPos] = useState<{ top: number; left: number; maxWidth: number } | null>(null)
  useLayoutEffect(() => {
    if (!open) return
    const b = ref.current?.getBoundingClientRect()
    if (!b) return
    const left = Math.max(8, Math.round(b.left))
    setPos({ top: Math.round(b.bottom + 6), left, maxWidth: Math.max(200, window.innerWidth - left - 8) })
  }, [open])
  const menuKeys = useMenuKeys(open, popRef, btnRef, () => setOpen(false))
  const selectBot = useStore((s) => s.selectBot)
  const projects = useStore((s) => s.projects)
  const bots = useStore((s) => s.bots)
  const projectOrder = useStore((s) => s.projectOrder)
  const botOrder = useStore((s) => s.botOrder)
  const hidden = useStore(useShallow((s) => s.hiddenBotIds))

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

  const groups = orderedProjects({ projects, projectOrder })
    .map((p) => {
      const all = botsOfProject({ bots, botOrder }, p.id)
      const ids = new Set(all.map((b) => b.id))
      return {
        project: p,
        bots: all.filter((b) => (!b.parent_bot_id || !ids.has(b.parent_bot_id)) && (b.id === botId || !hidden.includes(b.id))),
      }
    })
    .filter((g) => g.bots.length > 0)

  const currentBot = useStore((s) => s.bots.find((b) => b.id === botId))
  const currentProject = useStore((s) => s.projects.find((p) => p.id === currentBot?.project_id))
  const isExpertProject = currentProject?.mode === 'expert'

  return (
    <div className="bot-switcher" ref={ref}>
      <button
        ref={btnRef}
        type="button"
        className="bot-name-btn bot-switcher-btn"
        aria-haspopup="menu"
        aria-expanded={open}
        title={t('換一顆 bot')}
        onClick={() => setOpen((v) => !v)}
      >
        {isExpertProject && currentBot ? <ExpertRoleTag role={currentBot.role} rolePending={currentBot.role_pending} /> : null}
        <strong>{name}</strong>
        <span className="bot-switcher-chev" aria-hidden="true">
          ▾
        </span>
      </button>
      {}
      {open
        ? createPortal(
            <BotSwitcherMenu
              popRef={popRef}
              style={pos ? { top: pos.top, left: pos.left, maxWidth: pos.maxWidth } : { top: 0, left: 0, visibility: 'hidden' }}
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

function RowLamp({ botId }: { botId: string }) {
  const { lamp, background, kids } = useBotLamp(botId)
  return <StatusLamp lamp={lamp} background={background} kids={kids} />
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
  useT()
  return (
    <div ref={popRef} className="bot-switcher-pop" role="menu" aria-label={t('切換 bot')} style={style} onKeyDown={onKeyDown}>
      {groups.map((g) => {
        const isExpert = g.project.mode === 'expert'
        return (
          <div key={g.project.id} className="bot-switcher-group" role="group" aria-label={g.project.label}>
            <div className="bot-switcher-project" aria-hidden="true">⌗ {g.project.label}</div>
            {isExpert
              ? groupExpertSections(g.bots, g.project.deputy?.bot_id).map((sec) => (
                  <div key={sec.role} className="bot-switcher-section">
                    <div className="bot-switcher-expert-head" aria-hidden="true">
                      <span className="bot-switcher-expert-title">{sec.title}</span>
                      {sec.abbr && sec.abbr !== sec.title ? <span className="bot-switcher-expert-abbr">{sec.abbr}</span> : null}
                    </div>
                    {sec.items.map((b) => (
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
                        {g.project.deputy?.bot_id === b.id ? <ExpertDeputyTag afterSec={g.project.deputy.after_sec} /> : null}
                        {b.role_pending ? <ExpertRoleTag role={b.role} rolePending /> : null}
                        <span className="bot-switcher-name">{b.name}</span>
                      </button>
                    ))}
                  </div>
                ))
              : g.bots.map((b) => (
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
        )
      })}
    </div>
  )
}
