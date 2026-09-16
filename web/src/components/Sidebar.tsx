import { Fragment, useEffect, useMemo, useRef, useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import * as api from '../api'
import { MOCK_MODE } from '../api'
import type { Bot, BotKind, Lamp, MessageHit } from '../api/types'
import { BOT_KINDS, LOCAL_HOST } from '../api/types'
import {
  adjacentBotId,
  botLamp,
  botQuotaLevel,
  botQuotaWarning,
  botMatches,
  botsOfProject,
  identitiesOfHost,
  orderedProjects,
  projectHostName,
  toolsOfHost,
  useStore,
} from '../store/store'
import type { SocketStatus } from '../store/store'
import { quotaHiddenBotIds, useDisabledQuota } from '../store/quotaHide'
import { GearIcon, TerminalIcon } from './Icons'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'
import { ConfirmDialog } from './ConfirmDialog'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { projectDeleteBlockers } from './projectDeleteGuard'
import { HeadMoreMenu } from './HeadMoreMenu'
import { DirPicker } from './DirPicker'
import { IdentitiesPanel, IdentityBadge } from './IdentitiesPanel'
import { Modal } from './Modal'
import { SupervisorPanel } from './SupervisorPanel'
import { IdentityOptions, PersonaField, PersonaMark } from './BotSettingsPanel'
import { HostBadge, HostsPanel } from './HostsPanel'
import { BotNameField } from './BotNameField'
import { ProjectNameField } from './ProjectNameField'
import { MemBadge } from './MemBadge'
import { ThemeToggle } from './ThemeToggle'
import { ProjectMemBadge } from './ProjectMemBadge'
import { RebuildBadge } from './RebuildBadge'
import { TabsBadge } from './TabsBadge'
import { ModelTag } from './ModelTag'
import { BotRowMenu } from './BotRowMenu'
import { KIND_LABEL, KindDisplayToggle, KindTag } from './KindTag'
import { QuickAddBots } from './QuickAddBots'
import { UpdateAllBanner } from './UpdateAllBanner'
import { ApiModelFields } from './ModelPicker'
import { InstallToolButton } from './Tools'
import { UpdateBadge } from './UpdateBadge'
import { runtimeKnown } from '../lib/runtimeDrift'
import { syncKidsScroll, wheelKidsScroll } from '../lib/kidsScroll'
import { useWheelRef } from '../hooks/useWheelRef'
import './sidebar.css'

function ConnBadge({ socket, connected }: { socket: SocketStatus; connected: boolean }) {
  const label =
    socket === 'open' ? (connected ? '已連線' : 'herdr 中斷') : socket === 'connecting' ? '連線中' : '重連中'
  return (
    <span className="conn" title={`WebSocket: ${socket} / herdr: ${connected ? 'connected' : 'disconnected'}`}>
      <span className={`conn-dot ${socket === 'open' && !connected ? 'closed' : socket}`} />
      <span className="conn-label">{label}</span>
    </span>
  )
}

/** 開著幾個 herdr pane，和 RAM 並排；0 個就不出現（狀態燈已說明沒在跑）。 */
function PaneBadge() {
  // 回字串不回物件：useShallow 逐一 Object.is，新物件永不相等 → 無限重繪。
  const panes = useStore(
    useShallow((s) =>
      s.bots
        .filter((b) => {
          const r = s.runs[b.id]
          // 同 BotSettingsPanel 的判準。
          return Boolean(r) && r!.state !== 'stopped' && r!.state !== 'exited'
        })
        .map((b) => `${projectHostName(s, b.project_id)}\t${b.name}`),
    ),
  )
  if (panes.length === 0) return null

  const byHost = new Map<string, string[]>()
  for (const row of panes) {
    const [host, name] = row.split('\t')
    byHost.set(host, [...(byHost.get(host) ?? []), name])
  }
  const tip = [
    `現在開著 ${panes.length} 個 herdr pane`,
    '',
    ...[...byHost.entries()].map(([host, names]) => `${host === 'local' ? '本機' : host}：${names.join('、')}`),
  ]
  return (
    <span className="pane-badge" title={tip.join('\n')}>
      <span className="pane-k">pane</span>
      <span className="pane-v">{panes.length}</span>
    </span>
  )
}

/** Keep the tail of a long path readable without relying on RTL text tricks. */
function shortPath(path: string, max = 30): string {
  return path.length <= max ? path : `…${path.slice(-(max - 1))}`
}

/** 拖曳中的一列，以及游標落在哪一列的哪一半（插入線畫在那裡）。 */
type DragState = { id: string; projectId: string; overId: string | null; edge: 'before' | 'after' } | null

function BotRow({
  botId,
  hit,
  childCount = 0,
  collapsed = false,
  kidsLamp = null,
  kidsWait = null,
  kidsListId,
  compact = false,
  onToggleChildren,
  drag,
  onDrag,
  onDropAt,
  onNudge,
  onStep,
}: {
  botId: string
  /** 對話內容命中時的前後文。 */
  hit?: MessageHit
  childCount?: number
  collapsed?: boolean
  /** 收合時子 agent 最要緊的燈號；null 不畫。 */
  kidsLamp?: Lamp | null
  kidsWait?: KidsWait | null
  /** 子清單在 DOM 上是兄弟，用 aria-owns 掛回這一項。 */
  kidsListId?: string
  /** 子 agent 列：單行、只留身份／模型。 */
  compact?: boolean
  onToggleChildren?: () => void
  drag: DragState
  onDrag: (next: DragState) => void
  onDropAt: (dragId: string, overId: string, edge: 'before' | 'after') => void
  onNudge: (botId: string, dir: -1 | 1) => void
  onStep: (botId: string, dir: -1 | 1) => void
}) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId))
  const lamp = useStore((s) => botLamp(s, botId))
  // 額度 critical 時整列反灰＋警語（API.md §12.4）。botQuotaWarning 每次回新物件，不 useShallow 會無限重繪。
  const quotaWarning = useStore(
    useShallow((s) => {
      const b = s.bots.find((x) => x.id === botId)
      // 額度按主機分（SPEC §14）。
      return b ? botQuotaWarning(s.quota, b.kind, b.identity, projectHostName(s, b.project_id)) : null
    }),
  )
  // 黃燈（low）；同樣要 useShallow。
  const quotaLevel = useStore(
    useShallow((s) => {
      const b = s.bots.find((x) => x.id === botId)
      // 同 ModelTag：run 報的模型優先。
      const run = s.runs[botId] ?? null
      const model = run?.status?.model_name ?? (runtimeKnown(run) ? run!.runtime_model : (b?.model ?? null))
      return b ? botQuotaLevel(s.quota, b.kind, b.identity, projectHostName(s, b.project_id), model) : null
    }),
  )
  const selected = useStore((s) => s.selectedBotId === botId)
  const unread = useStore((s) => s.botUnread[botId] ?? 0)
  const selectBot = useStore((s) => s.selectBot)
  const agentTitle = useStore((s) => {
    const r = s.runs[botId]
    const t = r?.agent_title?.trim()
    if (!t) return ''
    const l = botLamp(s, botId)
    return l === 'working' || l === 'idle' ? t : ''
  })

  const hasUpdate = useStore((s) => s.runs[botId]?.update_notice ?? null)
  // 使用者 2026-09-10：子 bot 用不同帳號時標在母 bot 上，免得默默吃光另一帳號額度。未指定身分＝cc0。
  const divergedChildren = useStore(
    useShallow((s) => {
      const b = s.bots.find((x) => x.id === botId)
      if (!b || b.kind !== 'claude') return null
      const mine = b.identity ?? 'cc0'
      const kids = s.bots.filter((x) => x.parent_bot_id === botId && x.kind === 'claude' && (x.identity ?? 'cc0') !== mine)
      if (kids.length === 0) return null
      const identities = [...new Set(kids.map((x) => x.identity ?? 'cc0'))].sort()
      return { identities: identities.join('/'), names: kids.map((x) => x.name).join('、') }
    }),
  )
  // 回合被 API 斷線截斷：燈號仍是綠的，只有這個說「其實沒做完」。
  const turnError = useStore((s) => s.runs[botId]?.turn_error ?? null)

  if (!bot) return null
  // 佔位列：daemon 還沒建好，不能點、不能拖。
  if (bot.pending) {
    return (
      <div className="bot-row pending" role="listitem" aria-busy="true" data-bot-id={botId}>
        <StatusLamp lamp="starting" title={`${bot.name}：建立中`} />
        <span className="bot-main">
          <span className="bot-ident">
            <KindTag kind={bot.kind} className="bot-kind" />
            <span className="bot-name">{bot.name}</span>
          </span>
          <span className="bot-sub">
            <span className="bot-pending-note">建立中…</span>
          </span>
        </span>
      </div>
    )
  }
  // 標題只在選取列展開成一行，不打亂掃讀；子列留在 tooltip。
  const showTitle = Boolean(agentTitle) && selected && !compact

  const dragging = drag?.id === botId
  // 只在同專案內排序。
  const sameProject = drag?.projectId === bot.project_id
  const dropEdge = drag && sameProject && drag.id !== botId && drag.overId === botId ? drag.edge : null

  const edgeAt = (e: { currentTarget: HTMLElement; clientY: number }): 'before' | 'after' => {
    const r = e.currentTarget.getBoundingClientRect()
    return e.clientY < r.top + r.height / 2 ? 'before' : 'after'
  }

  return (
    <div
      className={`bot-row${compact ? ' compact' : ''}${selected ? ' selected' : ''}${dragging ? ' dragging' : ''}${
        dropEdge ? ` drop-${dropEdge}` : ''
      }${quotaWarning ? ' quota-critical' : ''}${childCount > 0 ? ' has-kids' : ''}`}
      // 不用 listbox option：option 內不准有按鈕，螢幕閱讀器會吃掉。
      role="listitem"
      aria-current={selected ? 'true' : undefined}
      aria-owns={kidsListId}
      // 手機子列不畫名字（styles.css），由這裡帶。
      aria-label={compact ? bot.name : undefined}
      data-bot-id={botId}
      tabIndex={0}
      draggable={!compact}
      onClick={() => selectBot(botId)}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          selectBot(botId)
        }
        if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return
        const dir = e.key === 'ArrowUp' ? -1 : 1
        // Alt+↑/↓ 鍵盤排序；子列跟父列走，與 draggable 一致。
        if (e.altKey) {
          e.preventDefault()
          if (!compact) onNudge(botId, dir)
          return
        }
        e.preventDefault()
        onStep(botId, dir)
      }}
      onDragStart={(e) => {
        e.dataTransfer.effectAllowed = 'move'
        e.dataTransfer.setData('text/plain', botId)
        onDrag({ id: botId, projectId: bot.project_id, overId: null, edge: 'before' })
      }}
      onDragEnd={() => onDrag(null)}
      onDragOver={(e) => {
        if (!drag || drag.id === botId || !sameProject) return
        e.preventDefault()
        e.dataTransfer.dropEffect = 'move'
        const edge = edgeAt(e)
        if (drag.overId !== botId || drag.edge !== edge) onDrag({ ...drag, overId: botId, edge })
      }}
      onDrop={(e) => {
        if (!drag || drag.id === botId || !sameProject) return
        e.preventDefault()
        onDropAt(drag.id, botId, edgeAt(e))
        onDrag(null)
      }}
    >
      {/* 燈號在上、收合鈕在正下方（使用者 2026-09-13），子列縮排才對齊父列。 */}
      <span className="bot-gutter">
        <span className="bot-gutter-top">
          <StatusLamp lamp={lamp} title={`${bot.name}：${LAMP_LABEL[lamp]}${agentTitle && !showTitle ? ` · ${agentTitle}` : ''}`} />
          {/* 黃點：紅色留給「要你本人回答」。 */}
          {kidsWait ? (
            <span
              className={`bot-kids-wait ${kidsWait}`}
              title={
                kidsWait === 'busy'
                  ? `${bot.name} 在等底下的子 agent 做完`
                  : `${bot.name} 的子 agent 回報了，還沒有人看`
              }
              aria-label={kidsWait === 'busy' ? '等子 agent 完成' : '子 agent 已回報'}
            />
          ) : null}
          {/* 未讀回合數，與燈號是兩件事；`!` 前綴免得截斷後被讀成模型參數。 */}
          {unread > 0 ? (
            <span className="unread-turns" title={`${unread} 個回合已完成，還沒看過`}>
              !{unread > 99 ? '99+' : unread}
            </span>
          ) : null}
        </span>
        {childCount > 0 && onToggleChildren ? (
        <button
          type="button"
          className={`bot-kids-toggle${collapsed ? ' shut' : ''}`}
          aria-expanded={!collapsed}
          title={
            collapsed
              ? `展開 ${childCount} 個子 agent${kidsLamp ? `（有子 agent ${LAMP_LABEL[kidsLamp]}）` : ''}`
              : `收合 ${childCount} 個子 agent`
          }
          onClick={(e) => {
            e.stopPropagation()
            onToggleChildren()
          }}
        >
          <span className="chev">{collapsed ? '▶' : '▼'}</span>
          {collapsed ? <span className="bot-kids-n">{childCount}</span> : null}
          {/* 收合時透出忙／卡住的子燈號。 */}
          {collapsed && kidsLamp ? <span className={`bot-kids-lamp lamp lamp-${kidsLamp}`} aria-hidden="true" /> : null}
        </button>
        ) : null}
      </span>
      <span className="bot-main" onScroll={compact ? syncKidsScroll : undefined}>
        <span className="bot-ident">
          {/* 新版記號在 kind icon 右上、綠色（使用者 2026-09-10）；可直接點（2026-09-11）。 */}
          <span className={`bot-kind-wrap${hasUpdate ? ' has-update' : ''}`}>
            <KindTag kind={bot.kind} className="bot-kind" />
            <UpdateBadge botId={botId} variant="dot" />
          </span>
          {/* 選取中的列，點名字才改名。 */}
          <BotNameField botId={botId} name={bot.name} variant="row" armed={selected}>
            {compact ? null : <PersonaMark persona={bot.persona} />}
            {/* 燈號說 idle 但回合是斷的，不能只留 tooltip；重送在 header chip。 */}
            {turnError ? (
              <span className="bot-turn-error" title={`${turnError}｜這一回合被 API 中斷，回應不完整。點進去可以重送上一則`}>
                ⚠ 中斷
              </span>
            ) : null}
            {showTitle || compact ? null : <span className={`bot-state ${lamp}`}>{LAMP_LABEL[lamp]}</span>}
          </BotNameField>
        </span>
        {hit ? (
          <span className="bot-hit" title={`對話中有 ${hit.hits} 則提到`}>
            <span className="bot-hit-n">{hit.hits}</span>
            <span className="bot-hit-text">{hit.snippet}</span>
          </span>
        ) : null}
        <span className="bot-sub">
          {/* 身份一定要標，同 CLI 兩帳號才分得出來。 */}
          {divergedChildren ? (
            <span
              className="identity-diverged"
              title={`底下有子 bot 用別的帳號（母 ${bot.identity ?? 'cc0'}、子 ${divergedChildren.identities}：${divergedChildren.names}）——額度分開算，注意別把那個帳號用光`}
            >
              <IdentityBadge name={bot.identity} showDefault kind={bot.kind} />
              <span className="identity-diverged-mark" aria-hidden="true">{divergedChildren.identities}</span>
            </span>
          ) : (
            <IdentityBadge name={bot.identity} showDefault kind={bot.kind} />
          )}
          {quotaWarning ? (
            // critical 警語取代模型標籤（側欄窄）；截斷時看 title。
            <span
              className="bot-quota-warn"
              title={`${KIND_LABEL[bot.kind]}${bot.identity ? ` · ${bot.identity}` : ''} ${quotaWarning.window} 額度剩 ${quotaWarning.pct}%，快用完了`}
            >
              ⚠ 額度剩 {quotaWarning.pct}%
            </span>
          ) : (
            <>
              <ModelTag botId={botId} />
              {/* 黃燈：與頂端 QuotaStrip 一致；critical 走上面的警語。 */}
              {quotaLevel ? (
                <span
                  className={`bot-quota-chip ${quotaLevel.level}`}
                  title={`${KIND_LABEL[bot.kind]}${bot.identity ? ` · ${bot.identity}` : ''} ${quotaLevel.window} 額度剩 ${quotaLevel.pct}%`}
                >
                  {quotaLevel.window} {quotaLevel.pct}%
                </span>
              ) : null}
            </>
          )}
        </span>
        {/* agent 標題獨佔一行：第二行擠進去會把模型截成 `op…`。 */}
        {showTitle ? (
          <span className="bot-agent-title" title={`agent 目前的標題：${agentTitle}`}>
            {agentTitle}
          </span>
        ) : null}
      </span>
      <BotRowMenu botId={botId} compact={compact} />

    </div>
  )
}

function NewProjectForm({ onDone }: { onDone: () => void }) {
  const addProject = useStore((s) => s.addProject)
  const hosts = useStore((s) => s.hosts)
  const [path, setPath] = useState('')
  const [label, setLabel] = useState('')
  const [host, setHost] = useState('local')
  const [busy, setBusy] = useState(false)
  const [browsing, setBrowsing] = useState(false)

  // A host removed while the form is open falls back to local.
  const hostOk = host === 'local' || hosts.some((h) => h.name === host)
  const effectiveHost = hostOk ? host : 'local'

  if (browsing) {
    return (
      <DirPicker
        initial={path}
        host={effectiveHost}
        onCancel={() => setBrowsing(false)}
        onPick={(p) => {
          setPath(p)
          if (!label.trim()) setLabel(p.split('/').filter(Boolean).pop() ?? '')
          setBrowsing(false)
        }}
      />
    )
  }

  return (
    <form
      className="form"
      onSubmit={(e) => {
        e.preventDefault()
        if (!path.trim() || busy) return
        setBusy(true)
        void addProject({ path: path.trim(), label: label.trim(), host: effectiveHost }).then((ok) => {
          setBusy(false)
          if (ok) {
            setPath('')
            setLabel('')
            onDone()
          }
        })
      }}
    >
      <label className="field">
        <span>主機</span>
        <select
          value={effectiveHost}
          onChange={(e) => {
            setHost(e.target.value)
            setPath('')
          }}
        >
          <option value="local">本機</option>
          {hosts.map((h) => (
            <option key={h.name} value={h.name} disabled={!h.connected}>
              {h.name}（{h.ssh}）{h.connected ? '' : ' — 未連線'}
            </option>
          ))}
        </select>
      </label>
      <label className="field">
        <span>目錄路徑{effectiveHost === 'local' ? '' : `（${effectiveHost} 上的絕對路徑）`}</span>
        <div className="field-with-btn">
          <input
            type="text"
            value={path}
            placeholder="/Users/me/project/foo"
            spellCheck={false}
            onChange={(e) => setPath(e.target.value)}
          />
          <button type="button" className="btn" onClick={() => setBrowsing(true)} title="瀏覽目錄">
            瀏覽…
          </button>
        </div>
      </label>
      <label className="field">
        <span>標籤（留白則取目錄名）</span>
        <input type="text" value={label} placeholder="foo" onChange={(e) => setLabel(e.target.value)} />
      </label>
      <div className="form-actions">
        <button type="button" className="btn" onClick={onDone}>
          取消
        </button>
        <button type="submit" className="btn primary" disabled={!path.trim() || busy}>
          新增
        </button>
      </div>
    </form>
  )
}

function lastKindKey(projectId: string): string {
  return `am:lastKind:${projectId}`
}

function uniqueBotName(kind: BotKind, projectId: string, bots: { project_id: string; name: string }[]): string {
  const taken = new Set(bots.filter((b) => b.project_id === projectId).map((b) => b.name))
  let n = 1
  while (taken.has(`${kind}-${n}`)) n += 1
  return `${kind}-${n}`
}

function defaultKindForProject(projectId: string, tools: ReturnType<typeof toolsOfHost>, bots: { project_id: string; kind: BotKind }[]): BotKind {
  const installed = BOT_KINDS.filter((k) => tools[k].installed)
  try {
    const stored = localStorage.getItem(lastKindKey(projectId)) as BotKind | null
    if (stored && installed.includes(stored)) return stored
  } catch {
    /* ignore */
  }
  for (let i = bots.length - 1; i >= 0; i -= 1) {
    const b = bots[i]
    if (b.project_id === projectId && tools[b.kind].installed) return b.kind
  }
  return installed[0] ?? BOT_KINDS[0]
}

function NewBotForm({ onDone, initialProjectId }: { onDone: () => void; initialProjectId?: string }) {
  const projects = useStore((s) => s.projects)
  const hosts = useStore((s) => s.hosts)
  const bots = useStore((s) => s.bots)
  const addBot = useStore((s) => s.addBot)
  const startBot = useStore((s) => s.startBot)
  const [projectId, setProjectId] = useState(initialProjectId ?? projects[0]?.id ?? '')
  const [projectFilter, setProjectFilter] = useState('')
  const pid = projectId || projects[0]?.id || ''
  const host = useStore((s) => projectHostName(s, pid || null))
  const tools = useStore((s) => toolsOfHost(s, host))
  const [kind, setKind] = useState<BotKind>(() => (pid ? defaultKindForProject(pid, tools, bots) : 'claude'))
  const [name, setName] = useState(() => (pid ? uniqueBotName(kind, pid, bots) : ''))
  const [model, setModel] = useState<string | null>(null)
  const [effort, setEffort] = useState<string | null>(null)
  const [fast, setFast] = useState(false)
  const [persona, setPersona] = useState('')
  const [identity, setIdentity] = useState('')
  const [busy, setBusy] = useState(false)
  const nameRef = useRef<HTMLInputElement>(null)
  const nameTouched = useRef(false)

  const nameOk = /^[^\s@,:;]{1,32}$/.test(name)
  const cliOk = Boolean(tools[kind]?.installed)
  const canSubmit = nameOk && cliOk && Boolean(pid) && !busy
  const hostUp = (h: string) => h === 'local' || (hosts.find((x) => x.name === h)?.connected ?? false)
  const filterQ = projectFilter.trim().toLowerCase()
  const visibleProjects = filterQ ? projects.filter((p) => p.label.toLowerCase().includes(filterQ)) : projects

  useEffect(() => {
    if (!pid) return
    const nextKind = defaultKindForProject(pid, tools, bots)
    setKind(nextKind)
    if (!nameTouched.current) setName(uniqueBotName(nextKind, pid, bots))
  }, [pid])

  useEffect(() => {
    nameRef.current?.focus()
  }, [])

  const pickKind = (k: BotKind) => {
    setKind(k)
    setIdentity('')
    setModel(null)
    setEffort(null)
    setFast(false)
    if (!nameTouched.current && pid) setName(uniqueBotName(k, pid, bots))
  }

  if (projects.length === 0) {
    return <p className="hint">請先新增一個 Project。</p>
  }

  return (
    <form
      className="form"
      onSubmit={(e) => {
        e.preventDefault()
        if (!canSubmit) return
        setBusy(true)
        void addBot(pid, {
          name,
          kind,
          model,
          effort,
          fast: kind === 'codex' ? fast : undefined,
          persona: persona.trim() || null,
          autostart: false,
          auto_approve: true,
          identity: kind === 'claude' && identity ? identity : null,
        }).then(async (id) => {
          setBusy(false)
          if (id) {
            try {
              localStorage.setItem(lastKindKey(pid), kind)
            } catch {
              /* ignore */
            }
            onDone()
            await startBot(id)
          }
        })
      }}
    >
      {initialProjectId ? null : (
        <div className="field">
          <span>Project</span>
          {projects.length > 6 ? (
            <input
              type="text"
              className="opt-filter"
              value={projectFilter}
              placeholder="過濾 Project…"
              aria-label="過濾 Project"
              spellCheck={false}
              onChange={(e) => setProjectFilter(e.target.value)}
            />
          ) : null}
          <div className="opt-group" role="radiogroup" aria-label="Project">
            {visibleProjects.map((p) => {
              const up = hostUp(p.host)
              return (
                <button
                  key={p.id}
                  type="button"
                  role="radio"
                  aria-checked={pid === p.id}
                  className={`opt${pid === p.id ? ' on' : ''}`}
                  disabled={!up}
                  title={up ? p.path : `${p.label}（主機未連線）`}
                  onClick={() => {
                    setProjectId(p.id)
                    nameTouched.current = false
                  }}
                >
                  <span className="opt-label">{p.label}</span>
                  <HostBadge host={p.host} connected={up} />
                </button>
              )
            })}
          </div>
          {visibleProjects.length === 0 ? <span className="hint">沒有符合的 Project。</span> : null}
        </div>
      )}
      <div className="field">
        <span>kind</span>
        <div className="opt-group kinds" role="radiogroup" aria-label="kind">
          {BOT_KINDS.map((k) => {
            const missing = !tools[k].installed
            const reason = missing ? `${host === 'local' ? '本機' : host} 尚未安裝 ${k}` : k
            return (
              <span key={k} className="opt-wrap">
                <button
                  type="button"
                  className={`opt${kind === k ? ' on' : ''}`}
                  disabled={missing}
                  title={reason}
                  onClick={() => pickKind(k)}
                >
                  <KindTag kind={k} />
                  <span className="opt-label">{k}</span>
                </button>
                {missing ? (
                  <>
                    <span className="kind-missing-reason">未安裝</span>
                    <InstallToolButton host={host} kind={k} small />
                  </>
                ) : null}
              </span>
            )
          })}
        </div>
      </div>
      <ApiModelFields
        kind={kind}
        host={host}
        identity={identity || null}
        model={model}
        onModel={setModel}
        effort={effort}
        onEffort={setEffort}
        fast={fast}
        onFast={setFast}
      />
      <IdentityOptions kind={kind} host={host} value={identity} onChange={setIdentity} />
      <label className="field">
        <span>名稱</span>
        <input
          ref={nameRef}
          type="text"
          value={name}
          placeholder={`${kind}-1`}
          spellCheck={false}
          onChange={(e) => {
            nameTouched.current = true
            setName(e.target.value)
          }}
        />
        {name && !nameOk ? <span className="hint">1–32 個字，不可含空白或 @ , : ;</span> : null}
        {!cliOk ? <span className="hint">此 kind 的 CLI 尚未安裝，無法建立</span> : null}
      </label>
      <PersonaField value={persona} onChange={setPersona} collapsible />
      <div className="form-actions">
        <button type="button" className="btn" onClick={onDone}>
          取消
        </button>
        <button type="submit" className="btn primary" disabled={!canSubmit}>
          {busy ? '建立中…' : '新增並啟動'}
        </button>
      </div>
    </form>
  )
}

/** SPEC §13.5/§13.6: title opens the group view; whole row is the hit area (v4.0, ≥ 32px). */
function ProjectTitle({
  projectId,
  label,
  host,
  path,
  hostUp,
  folded,
  draggable,
}: {
  projectId: string
  label: string
  host: string
  path: string
  hostUp: boolean
  /** 拖曳提示掛這裡不掛 header：header 的 title 會被 ＋/⋯ 繼承，與 data-tip 疊成兩個提示。 */
  draggable?: boolean
  /** 收合時把底下 bot 的未讀加總掛回標題。 */
  folded?: boolean
}) {
  const selected = useStore((s) => s.selectedProjectId === projectId)
  const unread = useStore((s) => s.groupUnread[projectId] ?? 0)
  const foldedUnread = useStore((s) =>
    folded ? s.bots.reduce((n, b) => (b.project_id === projectId ? n + (s.botUnread[b.id] ?? 0) : n), 0) : 0,
  )
  const selectProject = useStore((s) => s.selectProject)
  // `<input>` can't live in a `<button>`: renaming swaps the row for a same-class `<div>`.
  const [editing, setEditing] = useState(false)
  const inner = (
    <>
      <span className="project-group-icon" aria-hidden="true">
        ⌗
      </span>
      <ProjectNameField projectId={projectId} label={label} variant="row" armed={selected} editing={editing} onEditing={setEditing} />
      {unread > 0 ? (
        <span className="unread-badge" title={`${unread} 則未讀的群組回覆`}>
          {unread > 99 ? '99+' : unread}
        </span>
      ) : null}
      {foldedUnread > 0 ? (
        <span className="unread-turns" title={`收合的 Bot 裡有 ${foldedUnread} 個回合已完成，還沒看過`}>
          !{foldedUnread > 99 ? '99+' : foldedUnread}
        </span>
      ) : null}
      <HostBadge host={host} connected={hostUp} />
      <ProjectMemBadge projectId={projectId} />
      <span className="project-path" title={path}>
        {shortPath(path, 36)}
      </span>
    </>
  )
  if (editing) {
    return <div className={`project-label-btn${selected ? ' selected' : ''}`}>{inner}</div>
  }
  return (
    <button
      type="button"
      className={`project-label-btn${selected ? ' selected' : ''}`}
      title={`開啟「${label}」的群組聊天（@bot 或 @all 對多個 Bot 發言）\n${path}${draggable ? '\n拖曳可調整專案順序' : ''}`}
      // 不是開關，用 aria-current（同 bot 列）。
      aria-current={selected ? 'true' : undefined}
      onClick={(e) => {
        e.stopPropagation()
        selectProject(projectId)
      }}
    >
      {inner}
    </button>
  )
}

/** 父列在等子 agent（使用者 2026-09-12）：`busy` 子還在跑／卡住，`reply` 子回報了未讀。展開時也畫。 */
export type KidsWait = 'busy' | 'reply'

function kidsWaitOf(st: Parameters<typeof botLamp>[0], unread: Record<string, number>, ids: string[]): KidsWait | null {
  let out: KidsWait | null = null
  for (const id of ids) {
    const l = botLamp(st, id)
    if (l === 'working' || l === 'blocked') return 'busy'
    if ((unread[id] ?? 0) > 0) out = 'reply'
  }
  return out
}

/** 子 agent 最要緊的燈號：blocked > working，否則 null。 */
function kidsLampOf(st: Parameters<typeof botLamp>[0], ids: string[]): Lamp | null {
  let out: Lamp | null = null
  for (const id of ids) {
    const l = botLamp(st, id)
    if (l === 'blocked') return 'blocked'
    if (l === 'working') out = 'working'
  }
  return out
}

function kidsWheel(e: WheelEvent) {
  const el = e.currentTarget
  if (!(el instanceof HTMLElement)) return
  wheelKidsScroll({ currentTarget: el, deltaX: e.deltaX, deltaY: e.deltaY, shiftKey: e.shiftKey, preventDefault: () => e.preventDefault() })
}

/** 專案拖曳；與 bot 的 DragState 分開，互不干擾。 */
type ProjectDrag = { id: string; overId: string | null; edge: 'before' | 'after' } | null

export function Sidebar() {
  // 手機搜尋框 16px（iOS 聚焦不放大），placeholder 括號說明放不下。
  const phone = useMediaQuery(PHONE_QUERY)
  const rawProjects = useStore((s) => s.projects)
  const projectOrder = useStore((s) => s.projectOrder)
  const moveProject = useStore((s) => s.moveProject)
  const projects = useMemo(() => orderedProjects({ projects: rawProjects, projectOrder }), [rawProjects, projectOrder])
  const [pdrag, setPdrag] = useState<ProjectDrag>(null)
  const projectEdgeAt = (e: { currentTarget: HTMLElement; clientY: number }): 'before' | 'after' => {
    const r = e.currentTarget.getBoundingClientRect()
    return e.clientY < r.top + r.height / 2 ? 'before' : 'after'
  }
  const dropProjectAt = (dragId: string, overId: string, edge: 'before' | 'after') => {
    const ids = projects.map((p) => p.id)
    const at = ids.indexOf(overId)
    if (at < 0) return
    const beforeId = edge === 'before' ? overId : (ids[at + 1] ?? null)
    if (beforeId === dragId) return
    moveProject(dragId, beforeId)
  }
  const bots = useStore((s) => s.bots)
  const hosts = useStore((s) => s.hosts)
  const socket = useStore((s) => s.socket)
  const connected = useStore((s) => s.connected)
  const selectedProjectId = useStore((s) => s.selectedProjectId)
  const selectProject = useStore((s) => s.selectProject)
  const removeProject = useStore((s) => s.removeProject)
  const [open, setOpen] = useState<'project' | 'env' | 'agm' | null>(null)
  // config 身份＋本機 shell 認到的 ccN（SPEC §16）。
  const configuredIdentities = useStore((s) => s.identities)
  const localIdentityStatus = useStore((s) => s.localIdentityStatus)
  const identityCount = useMemo(
    () => identitiesOfHost(configuredIdentities, localIdentityStatus).length,
    [configuredIdentities, localIdentityStatus],
  )
  const [deleteProject, setDeleteProject] = useState<{ id: string; label: string } | null>(null)
  const runs = useStore((s) => s.runs)
  // botLamp 也讀 connected／hosts：只訂閱 runs 的話斷線後父列會殘留舊燈號到下一個 bot_status。
  const defaultConnected = useStore((s) => s.defaultConnected)
  const lampState = useMemo(
    () => ({ ...useStore.getState(), runs, hosts, connected, defaultConnected, bots, projects: rawProjects }),
    [runs, hosts, connected, defaultConnected, bots, rawProjects],
  )
  const unreadMap = useStore((s) => s.botUnread)
  const deleteTarget = deleteProject ? projects.find((p) => p.id === deleteProject.id) : undefined
  const deleteBlockers = deleteProject ? projectDeleteBlockers(bots, runs, deleteProject.id) : { total: 0, active: 0 }
  const [botFormFor, setBotFormFor] = useState<string | null>(null)
  const [botSheetOpen, setBotSheetOpen] = useState(false)
  const openBotSheetFor = useStore((s) => s.openBotSheetFor)
  const clearOpenBotSheet = useStore((s) => s.clearOpenBotSheet)
  const botOrder = useStore((s) => s.botOrder)
  const moveBot = useStore((s) => s.moveBot)
  // shift+滾輪要 preventDefault，React onWheel 是 passive（見 useWheelRef）。
  const kidsWheelRef = useWheelRef<HTMLDivElement>(kidsWheel)
  const [drag, setDrag] = useState<DragState>(null)
  const openHostShell = useStore((s) => s.openHostShell)
  /** 額度卡片上暫時停用的身分／kind，其 bot 先收起；reset 時 quotaHide 的 timer 會自動放回。 */
  const disabledQuota = useDisabledQuota()
  const hiddenQuotaIds = useStore(useShallow((s) => quotaHiddenBotIds(s, disabledQuota)))
  const hiddenQuota = useMemo(() => new Set(hiddenQuotaIds), [hiddenQuotaIds])
  const [collapsed, setCollapsed] = useState<Set<string>>(() => {
    try {
      const raw = localStorage.getItem('am.collapsedChildren')
      return new Set(raw ? (JSON.parse(raw) as string[]) : [])
    } catch {
      return new Set()
    }
  })
  const toggleChildren = (id: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev)
      if (!next.delete(id)) next.add(id)
      try {
        localStorage.setItem('am.collapsedChildren', JSON.stringify([...next]))
      } catch {
        /* 無痕視窗 / 關掉儲存：收合仍然有效，只是不跨重整記住 */
      }
      return next
    })
  const [shutProjects, setShutProjects] = useState<Set<string>>(() => {
    try {
      const raw = localStorage.getItem('am.collapsedProjects')
      return new Set(raw ? (JSON.parse(raw) as string[]) : [])
    } catch {
      return new Set()
    }
  })
  // 選取的 bot 換了才展開其專案並捲到可見；不搶焦點、不干擾手動捲動。
  const selectedBotId = useStore((s) => s.selectedBotId)
  const selectedBotProject = useStore((s) => s.bots.find((b) => b.id === s.selectedBotId)?.project_id ?? null)
  useEffect(() => {
    if (!selectedBotId) return
    if (selectedBotProject && shutProjects.has(selectedBotProject)) {
      setShutProjects((prev) => {
        if (!prev.has(selectedBotProject)) return prev
        const next = new Set(prev)
        next.delete(selectedBotProject)
        try {
          localStorage.setItem('am.collapsedProjects', JSON.stringify([...next]))
        } catch {
          /* 同上 */
        }
        return next
      })
    }
    // 展開後才進 DOM，等下一 frame。
    const id = requestAnimationFrame(() => {
      document.querySelector<HTMLElement>(`[data-bot-id="${selectedBotId}"]`)?.scrollIntoView({ block: 'nearest' })
    })
    return () => cancelAnimationFrame(id)
    // 故意不依賴 shutProjects：手動收合選取中的專案不該被彈開。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedBotId, selectedBotProject])
  const toggleProject = (id: string) =>
    setShutProjects((prev) => {
      const next = new Set(prev)
      if (!next.delete(id)) next.add(id)
      try {
        localStorage.setItem('am.collapsedProjects', JSON.stringify([...next]))
      } catch {
        /* 同上：收合仍然有效，只是不跨重整記住 */
      }
      return next
    })
  const [query, setQuery] = useState('')
  /** 內容命中問 daemon：debounce 250ms，seq 擋晚到的舊結果。 */
  const [hits, setHits] = useState<Record<string, MessageHit>>({})
  const hitSeq = useRef(0)
  useEffect(() => {
    const q = query.trim()
    if (!q) {
      setHits({})
      return
    }
    const mine = ++hitSeq.current
    const id = setTimeout(() => {
      void api
        .searchMessages(q)
        .then((r) => {
          if (hitSeq.current === mine) setHits(r)
        })
        // 失敗時屬性搜尋照常。
        .catch(() => {
          if (hitSeq.current === mine) setHits({})
        })
    }, 250)
    return () => clearTimeout(id)
  }, [query])
  const matches = (bot: Bot) => botMatches(useStore.getState(), bot, query) || bot.id in hits
  // 只數真的命中的，不含陪子 agent 顯示的父 bot。
  const matchCount = query ? bots.filter((b) => matches(b)).length : bots.length
  // 與 matchCount 同一群，免得數字自相矛盾。
  const hitCount = bots.filter((b) => b.id in hits).length

  const dropAt = (dragId: string, overId: string, edge: 'before' | 'after') => {
    const bot = bots.find((b) => b.id === overId)
    if (!bot) return
    const ids = botsOfProject({ bots, botOrder }, bot.project_id).map((b) => b.id)
    const at = ids.indexOf(overId)
    if (at < 0) return
    const beforeId = edge === 'before' ? overId : (ids[at + 1] ?? null)
    // 原地放下：不能傳 null（null＝移到最後）。
    if (beforeId === dragId) return
    moveBot(dragId, beforeId)
  }

  /** Alt+↑/↓ 換位置：只動父列，索引也只算父列（算進子列會原地不動）。 */
  const nudge = (botId: string, dir: -1 | 1) => {
    const bot = bots.find((b) => b.id === botId)
    if (!bot || bot.parent_bot_id) return
    const ids = botsOfProject({ bots, botOrder }, bot.project_id)
      .filter((b) => !b.parent_bot_id)
      .map((b) => b.id)
    const at = ids.indexOf(botId)
    const to = at + dir
    if (at < 0 || to < 0 || to >= ids.length) return
    moveBot(botId, dir === -1 ? ids[to] : (ids[to + 1] ?? null))
  }

  /** ↑/↓ 換 bot，焦點跟著走。 */
  const step = (botId: string, dir: -1 | 1) => {
    const st = useStore.getState()
    const next = adjacentBotId(st, botId, dir)
    if (!next) return
    st.selectBot(next)
    // The row for `next` may be a fresh element (or scrolled out); focus after React paints it.
    requestAnimationFrame(() => {
      const el = document.querySelector<HTMLElement>(`[data-bot-id="${next}"]`)
      el?.focus()
      el?.scrollIntoView({ block: 'nearest' })
    })
  }

  const hostUp = (name: string) => name === 'local' || (hosts.find((h) => h.name === name)?.connected ?? false)
  const hostsDown = hosts.filter((h) => !h.connected).length

  const closeBotSheet = () => {
    setBotFormFor(null)
    setBotSheetOpen(false)
  }

  const openBotSheet = (projectId?: string) => {
    setOpen(null)
    setBotFormFor(projectId ?? null)
    setBotSheetOpen(true)
  }

  useEffect(() => {
    if (!openBotSheetFor) return
    openBotSheet(openBotSheetFor)
    clearOpenBotSheet()
  }, [openBotSheetFor, clearOpenBotSheet])

  const botSheetProject = botFormFor ? projects.find((p) => p.id === botFormFor) : null
  const botSheetLabel = botSheetProject?.label ?? '選擇 Project'

  return (
    <>
      <div className="sidebar-head">
        <div className="head-brand">
          <div className="head-brand-title">
            <h1 title="Agents Manager">AG Man</h1>
            {MOCK_MODE ? <span className="mock-badge">MOCK</span> : null}
            {/* 重建申請數放在「AG Man」右上、連線燈號正上方（2026-09-14 使用者）。 */}
            <RebuildBadge />
          </div>
          <div className="head-brand-meta">
            <PaneBadge />
            <ThemeToggle />
            <ConnBadge socket={socket} connected={connected} />
          </div>
        </div>
        <div className="head-ram">
          <MemBadge />
        </div>
        <div className="head-right">
          <TabsBadge />
        </div>
      </div>

      {/* SPEC §6.9 */}
      <UpdateAllBanner />

      {/* 搜尋範圍見 store 的 botSearchText。 */}
      <div className="bot-search">
        <input
          type="search"
          className="bot-search-input"
          value={query}
          spellCheck={false}
          placeholder={phone ? '搜尋 bot…' : '搜尋 bot…（名稱、專案、模型、人設）'}
          aria-label="搜尋 bot"
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            // 只擋 Escape；其他鍵放行給 window 的 ⌥↑/⌥↓（FRONTEND.md）。
            if (e.key === 'Escape') {
              e.stopPropagation()
              e.preventDefault()
              setQuery('')
            }
          }}
        />
        {query ? (
          <span className="bot-search-count" title={hitCount ? `其中 ${hitCount} 個是對話內容命中` : undefined}>
            {matchCount} 個符合
            {hitCount ? <span className="bot-search-hits">（{hitCount} 個含對話）</span> : null}
            <button type="button" className="bot-search-clear" title="清除搜尋（Esc）" onClick={() => setQuery('')}>
              ✕
            </button>
          </span>
        ) : null}
      </div>

      {/* 不是 listbox：內含按鈕；各專案的 bot 各自是 list。 */}
      <nav className="sidebar-scroll" aria-label="Bot 清單">
        {query && matchCount === 0 ? (
          <p className="hint" style={{ padding: '12px 14px' }}>
            沒有符合「{query}」的 Bot。
          </p>
        ) : null}
        {/* 未連線時空清單是「還不知道」，不是「沒有專案」。 */}
        {projects.length === 0 && socket !== 'open' ? (
          <p className="hint" style={{ padding: '12px 14px' }}>
            正在連線 daemon，稍等一下就會列出 Project…
          </p>
        ) : projects.length === 0 ? (
          <p className="hint" style={{ padding: '12px 14px' }}>
            尚未設定任何 Project。請用下方的「新增 Project」開始。
          </p>
        ) : null}
        {projects.map((p) => {
          const every = botsOfProject({ bots, botOrder }, p.id)
          // 命中子 agent 連父 bot 一起留；父命中則帶出子 agent 當脈絡。
          const hit = new Set(every.filter((b) => matches(b)).map((b) => b.id))
          const all = every.filter(
            (b) =>
              hit.has(b.id) ||
              (b.parent_bot_id ? hit.has(b.parent_bot_id) : every.some((c) => c.parent_bot_id === b.id && hit.has(c.id))),
          )
          // 停用身分的 bot 收起；搜尋中不收。
          const shown = query ? all : all.filter((b) => !hiddenQuota.has(b.id))
          const hiddenCount = all.length - shown.length
          const list = shown.filter((b) => !b.parent_bot_id)
          const childrenOf = (id: string) => shown.filter((b) => b.parent_bot_id === id)
          const projectSelected = selectedProjectId === p.id
          // 搜尋中一律展開。
          const projectShut = shutProjects.has(p.id) && !query
          if (query && list.length === 0) return null
          const pDragging = pdrag?.id === p.id
          const pDropEdge = pdrag && pdrag.id !== p.id && pdrag.overId === p.id ? pdrag.edge : null
          return (
            <section
              className={`project${pDragging ? ' dragging' : ''}${pDropEdge ? ` drop-${pDropEdge}` : ''}`}
              key={p.id}
              /* Control+1…9 要把側欄捲到這個專案（`useProjectJumpKeys`）。 */
              data-project-id={p.id}
              onDragOver={(e) => {
                if (!pdrag || pdrag.id === p.id) return
                e.preventDefault()
                e.dataTransfer.dropEffect = 'move'
                const edge = projectEdgeAt(e)
                if (pdrag.overId !== p.id || pdrag.edge !== edge) setPdrag({ ...pdrag, overId: p.id, edge })
              }}
              onDrop={(e) => {
                if (!pdrag || pdrag.id === p.id) return
                e.preventDefault()
                dropProjectAt(pdrag.id, p.id, projectEdgeAt(e))
                setPdrag(null)
              }}
            >
              <header
                className={`project-head${projectSelected ? ' selected' : ''}`}
                // 滑鼠延伸命中區；鍵盤等價是 ProjectTitle 按鈕，故不給 tabIndex。
                onClick={() => selectProject(p.id)}
                draggable={!query}
                // 不在此掛 title：會被 ＋/⋯ 繼承（見 ProjectTitle）。
                onDragStart={(e) => {
                  e.stopPropagation()
                  e.dataTransfer.effectAllowed = 'move'
                  e.dataTransfer.setData('text/plain', `project:${p.id}`)
                  setPdrag({ id: p.id, overId: null, edge: 'before' })
                }}
                onDragEnd={() => setPdrag(null)}
              >
                {/* stopPropagation：不然會連帶選取專案。 */}
                <button
                  type="button"
                  className={`project-fold${projectShut ? ' shut' : ''}`}
                  aria-expanded={!projectShut}
                  title={projectShut ? `展開 ${p.label}（${all.length} 個 Bot）` : `收合 ${p.label}`}
                  onClick={(e) => {
                    e.stopPropagation()
                    toggleProject(p.id)
                  }}
                >
                  <span className="chev">{projectShut ? '▶' : '▼'}</span>
                </button>
                <ProjectTitle projectId={p.id} label={p.label} host={p.host} path={p.path} hostUp={hostUp(p.host)} folded={projectShut} draggable={!query} />
                <span className="project-head-actions">
                  <button
                    type="button"
                    className="icon-btn add icon-tip"
                    aria-label={`在 ${p.label} 新增 Bot`}
                    data-tip={`新增 Bot · ${p.label}`}
                    onClick={(e) => {
                      e.stopPropagation()
                      openBotSheet(p.id)
                    }}
                  >
                    ＋
                  </button>
                  {/* 刪除這種破壞性動作收進 ⋯，不跟 ＋ 並排。 */}
                  <HeadMoreMenu label={`更多動作 · ${p.label}`}>
                    <button
                      type="button"
                      className="head-menu-item"
                      role="menuitem"
                      disabled={!hostUp(p.host)}
                      title={
                        hostUp(p.host)
                          ? `在 ${p.host === 'local' ? '本機' : p.host} 的 ${p.path} 開一個 shell`
                          : '主機未連線，開不了 shell'
                      }
                      onClick={(e) => {
                        e.stopPropagation()
                        void openHostShell(p.host, p.path)
                      }}
                    >
                      在這裡開 shell
                    </button>
                    <button
                      type="button"
                      className="head-menu-item danger"
                      role="menuitem"
                      onClick={(e) => {
                        e.stopPropagation()
                        setDeleteProject({ id: p.id, label: p.label })
                      }}
                    >
                      刪除專案…
                    </button>
                  </HeadMoreMenu>
                </span>
              </header>
              {projectShut ? (
                <button type="button" className="project-folded" onClick={() => toggleProject(p.id)}>
                  {all.length} 個 Bot·點一下展開
                </button>
              ) : list.length === 0 && hiddenCount > 0 ? (
                // 全被隱藏：同空專案版面，但標「已隱藏」讓人知道 bot 沒不見。
                <div className="project-empty">
                  <span className="project-quota-hidden">{hiddenCount} 個 Bot 已隱藏（額度不足）</span>
                  <QuickAddBots projectId={p.id} />
                </div>
              ) : list.length === 0 ? (
                <div className="project-empty">
                  <span className="project-empty-title">此專案尚無 Bot</span>
                  <QuickAddBots projectId={p.id} />
                </div>
              ) : (
                // 子清單是父列的兄弟，靠 aria-owns 掛回，巢狀 list 才合法。
                <div className="bot-list" role="list" aria-label={`${p.label} 的 Bot`}>
                {list.map((b) => {
                  const kids = childrenOf(b.id)
                  // 搜尋中一律展開。
                  const shut = kids.length > 0 && collapsed.has(b.id) && !query
                  const kidsLamp = shut ? kidsLampOf(lampState, kids.map((c) => c.id)) : null
                  const kidsWait = kids.length ? kidsWaitOf(lampState, unreadMap, kids.map((c) => c.id)) : null
                  return (
                    <Fragment key={b.id}>
                      <BotRow
                        botId={b.id}
                        hit={hits[b.id]}
                        childCount={kids.length}
                        collapsed={shut}
                        kidsLamp={kidsLamp}
                        kidsWait={kidsWait}
                        kidsListId={shut || kids.length === 0 ? undefined : `bot-kids-${b.id}`}
                        onToggleChildren={() => toggleChildren(b.id)}
                        drag={drag}
                        onDrag={setDrag}
                        onDropAt={dropAt}
                        onNudge={nudge}
                        onStep={step}
                      />
                      {shut || kids.length === 0 ? null : (
                        <div id={`bot-kids-${b.id}`} className="bot-kids" role="list" aria-label={`${b.name} 的 ${kids.length} 個子 agent`} ref={kidsWheelRef}>
                          {kids.map((c, i) => (
                            <div key={c.id} className={`bot-child${i === kids.length - 1 ? ' last' : ''}`}>
                              <BotRow
                                compact
                                botId={c.id}
                                hit={hits[c.id]}
                                drag={drag}
                                onDrag={setDrag}
                                onDropAt={dropAt}
                                onNudge={nudge}
                                onStep={step}
                              />
                            </div>
                          ))}
                        </div>
                      )}
                    </Fragment>
                  )
                })}
                </div>
              )}
              {/* 腳註交代隱藏數，免得以為 bot 不見了。 */}
              {!projectShut && list.length > 0 && hiddenCount > 0 ? (
                <p className="project-quota-hidden">{hiddenCount} 個 Bot 已隱藏（額度不足）</p>
              ) : null}
            </section>
          )
        })}
      </nav>

      <div className="sidebar-foot">
        <div className="sidebar-foot-actions">
          <button type="button" className="btn" onClick={() => setOpen('project')}>
            新增 Project
          </button>
          <button type="button" className="btn" onClick={() => openBotSheet(selectedProjectId ?? projects[0]?.id)} disabled={projects.length === 0}>
            新增 Bot
          </button>
        </div>

        <button
          type="button"
          className="disclosure"
          title="在本機開一個 shell，直接下指令"
          onClick={() => void openHostShell(LOCAL_HOST)}
        >
          <TerminalIcon /> 開 shell
          <span className="disclosure-note">本機</span>
        </button>

        {/* 總管面板不是聊天室，對話從面板裡開。 */}
        <button
          type="button"
          className="disclosure"
          aria-haspopup="dialog"
          aria-expanded={open === 'agm'}
          onClick={() => setOpen('agm')}
        >
          <GearIcon /> AGM 總管
          <span className="disclosure-note">找先前做過的 bot、交辦與追蹤</span>
        </button>

        <button
          type="button"
          className="disclosure"
          aria-haspopup="dialog"
          aria-expanded={open === 'env'}
          onClick={() => setOpen('env')}
        >
          <GearIcon /> 環境設定
          <span className="disclosure-note">
            {hosts.length === 0 ? '本機' : `本機 + ${hosts.length}`}
            {hostsDown > 0 ? ` ・ ${hostsDown} 未連線` : ''}
            {` ・ 身分 ${identityCount}`}
          </span>
        </button>
      </div>

      <Modal open={open === 'project'} title="新增 Project" onClose={() => setOpen(null)}>
        <NewProjectForm onDone={() => setOpen(null)} />
      </Modal>

      <Modal
        open={botSheetOpen}
        title="新增 Bot"
        subtitle={<span title={botSheetProject?.path}>{botSheetLabel}</span>}
        onClose={closeBotSheet}
      >
        <NewBotForm key={botFormFor ?? 'pick'} initialProjectId={botFormFor ?? undefined} onDone={closeBotSheet} />
      </Modal>

      <ConfirmDialog
        open={deleteProject !== null}
        title="刪除 Project"
        body={
          <>
            要把 <strong>{deleteProject?.label}</strong>
            {deleteTarget ? (
              <>
                （{deleteTarget.host === LOCAL_HOST ? '本機' : deleteTarget.host} <code>{deleteTarget.path}</code>）
              </>
            ) : null}{' '}
            從清單移除嗎？磁碟上的目錄與其中的檔案都不會動
            {deleteBlockers.total > 0 ? `，底下 ${deleteBlockers.total} 個 Bot 的設定會一起移除` : ''}。
            {deleteBlockers.active > 0 ? (
              <>
                <br />
                <strong>仍有 {deleteBlockers.active} 個 Bot 在跑</strong>，需先停止才能刪除。
              </>
            ) : null}
          </>
        }
        confirmLabel="刪除 Project"
        confirmDisabled={deleteBlockers.active > 0}
        danger
        onCancel={() => setDeleteProject(null)}
        onConfirm={() => {
          const id = deleteProject?.id
          setDeleteProject(null)
          if (id) void removeProject(id)
        }}
      />

      <Modal open={open === 'agm'} title="AGM 總管" width={560} onClose={() => setOpen(null)}>
        <SupervisorPanel onOpenChat={() => setOpen(null)} />
      </Modal>

      <Modal open={open === 'env'} title="環境設定" width={560} onClose={() => setOpen(null)}>
        <div className="env-panel">
          <section className="env-sec">
            <h3>主機</h3>
            <HostsPanel />
          </section>
          <section className="env-sec">
            <h3>身分</h3>
            <IdentitiesPanel />
          </section>
          <section className="env-sec">
            <h3>顯示</h3>
            <KindDisplayToggle />
          </section>
        </div>
      </Modal>
    </>
  )
}
