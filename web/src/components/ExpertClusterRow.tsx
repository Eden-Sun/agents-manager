import { useEffect, useRef, useState, type ReactNode } from 'react'
import { useShallow } from 'zustand/react/shallow'
import { LOCAL_HOST, type Bot } from '../api/types'
import { identityBadgeVisible } from '../lib/identityBadgeVisible'
import { t, useT } from '../lib/i18n'
import { runningFast, runtimeIdentity } from '../lib/runtimeDrift'
import { clusterStatCounts, clusterStatText } from '../lib/expertClusters'
import { botLamp, identitiesOfHost, projectHostName, useStore } from '../store/store'
import { IdentityBadge } from './IdentitiesPanel'
import { MoreIcon, NoDispatchIcon } from './Icons'
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

/**
 * 群集的停派計數是狀態，不是勾選框。整組切換在旁邊的 `⋯`。
 * 有人停派才顯示「{n} 停派」；選單不冒泡，免得同時展開群集。
 */
function ClusterManagerUseChip({ members, on, busy }: { members: readonly Bot[]; on: number; busy: boolean }) {
  useT()
  const [open, setOpen] = useState(false)
  const wrapRef = useRef<HTMLSpanElement>(null)
  const total = members.length
  const off = total - on
  const allOn = off === 0

  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('mousedown', onDown)
    return () => document.removeEventListener('mousedown', onDown)
  }, [open])

  // 只送差異成員的 PATCH；失敗的計數回報，全部成功不跳通知。
  const setAll = (next: boolean) => {
    setOpen(false)
    const targets = members.filter((m) => (m.manager_use !== false) !== next)
    if (targets.length === 0) return
    const patch = useStore.getState().patchBot
    void Promise.allSettled(targets.map((m) => patch(m.id, { manager_use: next }))).then((rs) => {
      const fail = rs.filter((r) => r.status === 'rejected' || r.value === null).length
      if (fail > 0) useStore.getState().notify('error', t('{ok} 顆已更新，{fail} 顆沒成功', { ok: targets.length - fail, fail }))
    })
  }

  const stop = (e: { stopPropagation: () => void }) => e.stopPropagation()
  return (
    <span
      ref={wrapRef}
      className="bot-cluster-chip-wrap"
      onClick={stop}
      onMouseDown={stop}
      onKeyDown={(e) => {
        stop(e)
        if (e.key === 'Escape') setOpen(false)
      }}
    >
      {off > 0 ? (
        <span
          className="bot-cluster-pause"
          title={t('{n} 顆停派，共 {total} 顆。管理者不能派工給它們；Bot 本身不會停。', { n: off, total })}
        >
          <NoDispatchIcon />
          <span>{t('{n} 停派', { n: off })}</span>
        </span>
      ) : null}
      <button
        type="button"
        className="bot-cluster-menu"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={t('整組停派或允許派工')}
        title={t('整組停派或允許派工')}
        disabled={busy}
        onClick={() => setOpen((v) => !v)}
      >
        <MoreIcon />
      </button>
      {open ? (
        <span className="bot-unset-role-menu bot-cluster-chip-menu" role="menu">
          <button type="button" role="menuitem" disabled={allOn} onClick={() => setAll(true)}>{t('全部允許派工')}</button>
          <button type="button" role="menuitem" disabled={on === 0} onClick={() => setAll(false)}>{t('全部停派')}</button>
          <span className="bot-cluster-chip-note">{t('停派：管理者不能派工、啟動或重啟；Bot 本身不會停。')}</span>
        </span>
      ) : null}
    </span>
  )
}

/** 一列「<name> 群集 ●1 ○2」：標題後面是各狀態的燈號＋數量，右邊是停派晶片；可展開看成員。 */
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
  const listId = `bot-cluster-${projectId}-${name}`

  return (
    <>
      <div className={`bot-row bot-cluster-row${on === 0 && members.length > 0 ? ' manager-disabled' : ''}`} role="listitem" data-cluster={name}>
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
              {/* 狀態與數量合併（使用者 2026-11-04）：不再另起一行「1 執行中 · 2 閒置」，燈色同單顆列。 */}
              <span
                className="bot-cluster-count"
                role="img"
                aria-label={t('共 {n} 顆：{stat}', { n: members.length, stat: clusterStatText(lamps) })}
                title={t('共 {n} 顆：{stat}', { n: members.length, stat: clusterStatText(lamps) })}
              >
                {clusterStatCounts(lamps).map(({ stat, n }) => (
                  <span key={stat} className="bot-cluster-stat-item">
                    <span className={`lamp lamp-${stat === 'stopped' ? 'offline' : stat} bot-cluster-dot`} aria-hidden="true" />
                    {n}
                  </span>
                ))}
              </span>
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
          </span>
        </button>
        <ClusterManagerUseChip members={members} on={on} busy={busyPatch} />
      </div>
      {open ? (
        <div id={listId} className="bot-cluster-members" role="list">
          {children}
        </div>
      ) : null}
    </>
  )
}
