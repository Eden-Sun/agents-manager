import { useEffect, useRef, useState, type ReactNode } from 'react'
import { useShallow } from 'zustand/react/shallow'
import { LOCAL_HOST, type Bot } from '../api/types'
import { identityBadgeVisible } from '../lib/identityBadgeVisible'
import { t, useT } from '../lib/i18n'
import { runningFast, runtimeIdentity } from '../lib/runtimeDrift'
import { clusterStatText } from '../lib/expertClusters'
import { botLamp, identitiesOfHost, projectHostName, useStore } from '../store/store'
import { IdentityBadge } from './IdentitiesPanel'
import { KindTag } from './KindTag'
import { ModelTag } from './ModelTag'

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

/** 一列「<name> 群集 ×N」，第三行列出各狀態的數量（加總等於成員數），可展開看成員；勾選框一次切整組。 */
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
  const busyPatch = useStore(useShallow((s) => ids.some((id) => Boolean(s.busy[`patch:${id}`]))))

  // 列上的 kind／身份／模型取第一顆成員（群集同設定）；身份晶片的顯示規則同單顆列（BotRowImpl）。
  const first = members[0]
  const firstId = first?.id ?? ''
  const firstRun = useStore((s) => s.runs[firstId] ?? null)
  const runningIdentity = runtimeIdentity(firstRun)
  const identityUnknown = Boolean(firstRun && (firstRun.state === 'running' || firstRun.state === 'starting') && firstRun.runtime_identity === null)
  const showIdentity = useStore(
    useShallow((s) => {
      const b = s.bots.find((x) => x.id === firstId)
      if (!b) return false
      const host = projectHostName(s, b.project_id)
      const hostIds = identitiesOfHost(s.identities, host === LOCAL_HOST ? s.localIdentityStatus : {}, host)
      const used = s.bots.filter((x) => x.kind === b.kind && projectHostName(s, x.project_id) === host).map((x) => x.identity)
      return identityBadgeVisible(b.kind, hostIds, used)
    }),
  )
  const fast = useStore((s) => first?.kind === 'codex' && runningFast(first, s.runs[firstId] ?? null))

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
        {/* 跟成員列同一欄：勾選框在最左。整組仍可全開或全關，不鎖。 */}
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
        <button
          type="button"
          className="bot-cluster-main"
          aria-expanded={open}
          aria-controls={listId}
          onClick={toggleOpen}
        >
          <span className="bot-cluster-caret" aria-hidden="true">{open ? '▾' : '▸'}</span>
          {first ? (
            <span className="bot-ident">
              <span className="bot-kind-wrap">
                <KindTag kind={first.kind} className="bot-kind" fast={fast} />
              </span>
            </span>
          ) : null}
          <span className="bot-main">
            <span className="bot-cluster-head">
              <span className="bot-cluster-name">{t('{name} 群集', { name })}</span>
              <span className="bot-cluster-count">×{members.length}</span>
            </span>
            <span className="bot-sub">
              {first && showIdentity ? (
                <IdentityBadge
                  name={runningIdentity === undefined ? first.identity : runningIdentity}
                  showDefault
                  kind={first.kind}
                  unknown={identityUnknown}
                />
              ) : null}
              {first ? <ModelTag botId={first.id} /> : null}
            </span>
            <span className="bot-cluster-stat">{clusterStatText(lamps)}</span>
          </span>
        </button>
      </div>
      {open ? (
        <div id={listId} className="bot-cluster-members" role="list">
          {children}
        </div>
      ) : null}
    </>
  )
}
