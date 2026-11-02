import { useEffect, useRef, useState, type ReactNode } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Bot } from '../api/types'
import { t, useT } from '../lib/i18n'
import { botLamp, useStore } from '../store/store'

const storeKey = (projectId: string, name: string) => `agm.expertCluster.${projectId}.${name}`

function readOpen(key: string): boolean {
  try {
    return window.localStorage.getItem(key) === '1'
  } catch {
    return false
  }
}

function writeOpen(key: string, open: boolean) {
  try {
    window.localStorage.setItem(key, open ? '1' : '0')
  } catch {
    // localStorage 不可用就只在這次畫面記著。
  }
}

/** 一列「<name> 群集 ×N　X 執行中 · Y 閒置」，可展開看成員；勾選框一次切整組。 */
export function ExpertClusterRow({
  projectId,
  name,
  members,
  children,
}: {
  projectId: string
  name: string
  members: readonly Bot[]
  /** 展開後的成員列（呼叫端用原本的 BotRow 畫）。 */
  children: ReactNode
}) {
  useT()
  const key = storeKey(projectId, name)
  const [open, setOpen] = useState(() => readOpen(key))
  const toggleOpen = () => {
    setOpen((v) => {
      writeOpen(key, !v)
      return !v
    })
  }

  const ids = members.map((m) => m.id)
  const lamps = useStore(useShallow((s) => ids.map((id) => botLamp(s, id))))
  const working = lamps.filter((l) => l === 'working').length
  const idle = lamps.filter((l) => l === 'idle').length
  const busyPatch = useStore(useShallow((s) => ids.some((id) => Boolean(s.busy[`patch:${id}`]))))

  const on = members.filter((m) => m.manager_use !== false).length
  const all = on === members.length
  const some = on > 0 && !all
  const boxRef = useRef<HTMLInputElement>(null)
  useEffect(() => {
    if (boxRef.current) boxRef.current.indeterminate = some
  }, [some])

  const title = t('管理者可用：開啟後管理者可派工、啟動或重啟此成員')
  const setAll = (next: boolean) => {
    const patch = useStore.getState().patchBot
    for (const m of members) void patch(m.id, { manager_use: next }).catch(() => undefined)
  }
  const listId = `bot-cluster-${projectId}-${name}`

  return (
    <>
      <div className="bot-row bot-cluster-row" role="listitem" data-cluster={name}>
        <button
          type="button"
          className="bot-cluster-main"
          aria-expanded={open}
          aria-controls={listId}
          onClick={toggleOpen}
        >
          <span className="bot-cluster-caret" aria-hidden="true">{open ? '▾' : '▸'}</span>
          <span className="bot-cluster-name">{t('{name} 群集', { name })}</span>
          <span className="bot-cluster-count">×{members.length}</span>
          <span className="bot-cluster-stat">{t('{w} 執行中 · {i} 閒置', { w: working, i: idle })}</span>
        </button>
        <span
          className="bot-manager-use-wrap"
          onClick={(e) => e.stopPropagation()}
          onMouseDown={(e) => e.stopPropagation()}
          onKeyDown={(e) => e.stopPropagation()}
        >
          <input
            ref={boxRef}
            type="checkbox"
            className="bot-manager-use-checkbox"
            aria-label={title}
            title={title}
            checked={all}
            disabled={busyPatch}
            onChange={() => setAll(!all)}
          />
        </span>
      </div>
      {open ? (
        <div id={listId} className="bot-cluster-members" role="list">
          {children}
        </div>
      ) : null}
    </>
  )
}
