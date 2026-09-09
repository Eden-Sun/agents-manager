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
import { IdentityOptions, PersonaField, PersonaMark } from './BotSettingsPanel'
import { HostBadge, HostsPanel } from './HostsPanel'
import { BotNameField } from './BotNameField'
import { ProjectNameField } from './ProjectNameField'
import { MemBadge } from './MemBadge'
import { TabsBadge } from './TabsBadge'
import { ModelTag } from './ModelTag'
import { BotRowMenu } from './BotRowMenu'
import { KIND_LABEL, KindDisplayToggle, KindTag } from './KindTag'
import { QuickAddBots } from './QuickAddBots'
import { UpdateAllBanner } from './UpdateAllBanner'
import { ApiModelFields } from './ModelPicker'
import { TeamNodes } from './TeamNodes'
import { InstallToolButton } from './Tools'

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

/**
 * 現在開著幾個 herdr pane（＝有幾個 bot 的終端還活著）。和 RAM 那格並排：兩個都是
 * 「整套系統現在佔多少」，一個講記憶體、一個講終端；哪一台開了幾個放 tooltip。
 *
 * 一個都沒有就整格不出現——`pane 0` 只是佔位置，狀態燈已經說了沒有東西在跑。
 */
function PaneBadge() {
  // 字串而不是物件：`useShallow` 是逐一 `Object.is`，每次都給新物件就永遠不相等，
  // 於是每次 render 都算「變了」→ 無限重繪。
  const panes = useStore(
    useShallow((s) =>
      s.bots
        .filter((b) => {
          const r = s.runs[b.id]
          // 和 BotSettingsPanel 同一條判準：pane 還在就算開著。
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
  compact = false,
  onToggleChildren,
  drag,
  onDrag,
  onDropAt,
  onNudge,
  onStep,
}: {
  botId: string
  /** 這一列是因為對話內容命中而留下來的話，把命中的前後文一起顯示。 */
  hit?: MessageHit
  /** 這個 bot 底下有幾個子 agent；0 = 不顯示收合鈕。 */
  childCount?: number
  collapsed?: boolean
  /** 收合時底下子 agent 最要緊的燈號（blocked > working）；null = 沒有在忙的，不畫。 */
  kidsLamp?: Lamp | null
  /** 子 agent 列：單行、不重複 kind，只留身份／模型。 */
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
  // SPEC：bot 對應額度 critical（daemon 算好，見 docs/API.md §12.4）時，整列反灰＋警語。
  // `botQuotaWarning` 每次都 new 一個新物件，跟 ChatPanel 的 `composerState` 同一個坑
  // （見 ChatPanel.tsx 的 `useShallow(composerState)`）——要淺比較，否則 useSyncExternalStore 會判斷
  // 每次快照都變了而無限重渲染／噴 getSnapshot 警告，整個側欄的 bot 列都不會 render。
  const quotaWarning = useStore(
    useShallow((s) => {
      const b = s.bots.find((x) => x.id === botId)
      // 額度按主機分（SPEC §14）：遠端 bot 要看它自己那台的數字，不是本機的。
      return b ? botQuotaWarning(s.quota, b.kind, b.identity, projectHostName(s, b.project_id)) : null
    }),
  )
  // 黃燈（low）也要在側欄看得到：同一個淺比較的坑，一樣用 useShallow。
  const quotaLevel = useStore(
    useShallow((s) => {
      const b = s.bots.find((x) => x.id === botId)
      return b ? botQuotaLevel(s.quota, b.kind, b.identity, projectHostName(s, b.project_id)) : null
    }),
  )
  const selected = useStore((s) => s.selectedBotId === botId)
  // 已完成但還沒被看到的回合數（store/unread.ts）。0 = 不佔位。
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
  // 上一回合被 API 斷線截斷（`runs.turn_error`）。燈號是綠的，這顆才是「其實沒做完」。
  const turnError = useStore((s) => s.runs[botId]?.turn_error ?? null)

  if (!bot) return null
  // 佔位列：分身剛按下去、daemon 還沒建好。灰的、不能點、不能拖，只告訴你「它會出現在這裡」。
  if (bot.pending) {
    return (
      <div className="bot-row pending" role="option" aria-selected={false} aria-busy="true" data-bot-id={botId}>
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
  // 標題只在選取中的那一列展開成一行——一次只有一列，清單的掃讀節奏不會被打亂。
  // 子 agent 列是單行，標題留在 tooltip，不把樹撐高。
  const showTitle = Boolean(agentTitle) && selected && !compact

  const dragging = drag?.id === botId
  // 只在同一個專案內排序：跨專案拖曳不畫插入線，也不會有動作。
  const sameProject = drag?.projectId === bot.project_id
  const dropEdge = drag && sameProject && drag.id !== botId && drag.overId === botId ? drag.edge : null

  /** 落點：拖到上半 = 插在這列之前，下半 = 插在這列之後。 */
  const edgeAt = (e: { currentTarget: HTMLElement; clientY: number }): 'before' | 'after' => {
    const r = e.currentTarget.getBoundingClientRect()
    return e.clientY < r.top + r.height / 2 ? 'before' : 'after'
  }

  return (
    <div
      className={`bot-row${compact ? ' compact' : ''}${selected ? ' selected' : ''}${dragging ? ' dragging' : ''}${
        dropEdge ? ` drop-${dropEdge}` : ''
      }${quotaWarning ? ' quota-critical' : ''}${childCount > 0 ? ' has-kids' : ''}`}
      role="option"
      aria-selected={selected}
      // 手機的子列不畫名字（見 styles.css），名字改由這裡帶著走。
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
        // 鍵盤也要能排序：Alt + ↑/↓（拖曳不是每個人都能用）。只有父列可以換位置——
        // 子 agent 跟著父列走，拖曳同樣不開放（`draggable={!compact}`），兩邊一致。
        if (e.altKey) {
          e.preventDefault()
          if (!compact) onNudge(botId, dir)
          return
        }
        // 沒按 Alt 就是換 bot——listbox 本來就該這樣走，焦點跟著跳到新的那一列。
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
      {/* 沒展開標題的列，agent 的標題掛在燈號的 tooltip 上，資訊沒有掉。 */}
      {/* 收合鈕排在燈號前面。收起來時把數量帶上——不然收合後就看不出底下還有東西。 */}
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
          {/* 收起來時子 agent 的燈號跟著藏了；還在忙／卡住的那顆要透出來，不然收合等於把它藏掉。 */}
          {collapsed && kidsLamp ? <span className={`bot-kids-lamp lamp lamp-${kidsLamp}`} aria-hidden="true" /> : null}
        </button>
      ) : null}
      <StatusLamp lamp={lamp} title={`${bot.name}：${LAMP_LABEL[lamp]}${agentTitle && !showTitle ? ` · ${agentTitle}` : ''}`} />
      {/* 「已完成（未讀）」：燈號說的是**現在**在做什麼，這顆說的是**你還沒看過**幾回合——
          兩件不同的事，所以是兩個記號，並排在名字前面。`!` 讓它就算被截斷也不會被讀成模型參數。 */}
      {unread > 0 ? (
        <span className="unread-turns" title={`${unread} 個回合已完成，還沒看過`}>
          !{unread > 99 ? '99+' : unread}
        </span>
      ) : null}
      <span className="bot-main">
        <span className="bot-ident">
          <KindTag kind={bot.kind} className="bot-kind" />
          {/* 選取中的那一列，名字點下去就改名（未選取的第一下還是「開啟這個 bot」）。 */}
          <BotNameField botId={botId} name={bot.name} variant="row" armed={selected}>
            {compact ? null : <PersonaMark persona={bot.persona} />}
            {/* agent 自己的標題不再跟名字擠同一行——那樣兩邊各剩六個字
                （`C0-畫面修改者 資料夾…`）。選取中的那一列給它自己一行（見下面），
                其餘的列名字獨佔第一行，標題在整列的 tooltip 裡。 */}
            {/* claude 有新版等著重啟套用時的小點。只是提示——真正點得下去的那顆在 header
                （`UpdateBadge`），側欄這裡窄到放不下一顆按鈕。 */}
            {hasUpdate ? (
              <span className="bot-update-dot" aria-label="有更新，重啟套用" title={`${hasUpdate}｜重啟這個 bot 會用新版 claude 接著跑（session 會 --resume）`}>
                ⬆
              </span>
            ) : null}
            {/* 側欄放不下一顆按鈕，但這件事不能只留在 tooltip：燈號說 idle、實際上回合是斷的。
                所以給它一個看得見的紅記號，點進去 header 那顆 chip 有原文與「重送上一則」。 */}
            {turnError ? (
              <span className="bot-turn-error" title={`${turnError}｜這一回合被 API 中斷，回應不完整。點進去可以重送上一則`}>
                ⚠ 中斷
              </span>
            ) : null}
            {showTitle || compact ? null : <span className={`bot-state ${lamp}`}>{LAMP_LABEL[lamp]}</span>}
          </BotNameField>
        </span>
        {/* 對話內容命中時，把命中的那一段秀出來——只說「命中」不告訴你命中什麼，
            等於要你一個一個點進去確認。 */}
        {hit ? (
          <span className="bot-hit" title={`對話中有 ${hit.hits} 則提到`}>
            <span className="bot-hit-n">{hit.hits}</span>
            <span className="bot-hit-text">{hit.snippet}</span>
          </span>
        ) : null}
        <span className="bot-sub">
          {/* 身份（cc0 / cc1…）一定要標，同一個 CLI 兩個帳號才分得出來。 */}
          <IdentityBadge name={bot.identity} showDefault kind={bot.kind} />
          {quotaWarning ? (
            // 額度 critical：警語取代模型標籤（側欄窄，優先顯示這個）；文字撐不下就截斷，完整內容看 title。
            <span
              className="bot-quota-warn"
              title={`${KIND_LABEL[bot.kind]}${bot.identity ? ` · ${bot.identity}` : ''} ${quotaWarning.window} 額度剩 ${quotaWarning.pct}%，快用完了`}
            >
              ⚠ 額度剩 {quotaWarning.pct}%
            </span>
          ) : (
            <>
              <ModelTag botId={botId} />
              {/* 額度黃燈：頂端 QuotaStrip 已經黃了，側欄不提示等於兩套數字。只在還沒到
                  critical 時出現（critical 走上面那條警語，不重複佔位）。 */}
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
        {/* agent 對自己工作的一句話（claude 的 pane 標題）。只有選取中的那一列給它一整行：
            側欄第二行已經被 kind / 身份 / 模型三顆徽章佔滿，硬擠進去只會把模型也截成 `op…`。 */}
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

  // A host removed while the form is open falls back to the local machine.
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

/**
 * SPEC §13.5/§13.6: the project title opens the group view; unread replies pile up on it.
 * v4.0: the whole row (label + host + path) is the hit area, ≥ 32px tall.
 */
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
  /** 標題列是專案的拖曳把手（搜尋中不能拖）；提示掛在這顆鍵上而不是外層 header——
   *  header 的 `title` 會被 `＋` / `⋯` 繼承，跟它們的 `data-tip` 泡泡疊成兩個提示。 */
  draggable?: boolean
  /**
   * 專案收起來時，底下每個 bot 的未讀加總掛回標題上——不然收合等於把徽章藏起來。
   * 選擇性：收合是側欄自己的本地狀態，沒傳就是沒收合。
   */
  folded?: boolean
}) {
  const selected = useStore((s) => s.selectedProjectId === projectId)
  const unread = useStore((s) => s.groupUnread[projectId] ?? 0)
  const foldedUnread = useStore((s) =>
    folded ? s.bots.reduce((n, b) => (b.project_id === projectId ? n + (s.botUnread[b.id] ?? 0) : n), 0) : 0,
  )
  const selectProject = useStore((s) => s.selectProject)
  // An `<input>` may not live inside a `<button>`, so renaming swaps the whole row for a
  // plain `<div>` wearing the same class — the row keeps its size and the field gets focus.
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
      aria-pressed={selected}
      onClick={(e) => {
        e.stopPropagation()
        selectProject(projectId)
      }}
    >
      {inner}
    </button>
  )
}

/** 一群子 agent 裡最要緊的燈號：blocked > working；都不是就 null（idle / done 不值得在父列上亮）。 */
function kidsLampOf(st: Parameters<typeof botLamp>[0], ids: string[]): Lamp | null {
  let out: Lamp | null = null
  for (const id of ids) {
    const l = botLamp(st, id)
    if (l === 'blocked') return 'blocked'
    if (l === 'working') out = 'working'
  }
  return out
}

/** 拖曳中的專案，以及游標落在哪個專案的哪一半。與 bot 的 `DragState` 分開：兩種拖曳不互相干擾。 */
type ProjectDrag = { id: string; overId: string | null; edge: 'before' | 'after' } | null

export function Sidebar() {
  // 搜尋框在 ≤640 是 16px（iOS 聚焦不放大），括號那半句就放不下、會被切在字中間。
  // 括號裡本來也只是說明「搜尋範圍不只名字」，手機少一行說明比多半個字好。
  const phone = useMediaQuery(PHONE_QUERY)
  const rawProjects = useStore((s) => s.projects)
  const projectOrder = useStore((s) => s.projectOrder)
  const moveProject = useStore((s) => s.moveProject)
  const projects = useMemo(() => orderedProjects({ projects: rawProjects, projectOrder }), [rawProjects, projectOrder])
  const [pdrag, setPdrag] = useState<ProjectDrag>(null)
  /** 專案落點：上半 = 插在這個專案之前，下半 = 之後。 */
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
  const [open, setOpen] = useState<'project' | 'env' | null>(null)
  // config 的身份加上本機 shell 認到的 `ccN`（SPEC §16）——腳註寫的是「這台機器有幾個身份」。
  const configuredIdentities = useStore((s) => s.identities)
  const localIdentityStatus = useStore((s) => s.localIdentityStatus)
  const identityCount = useMemo(
    () => identitiesOfHost(configuredIdentities, localIdentityStatus).length,
    [configuredIdentities, localIdentityStatus],
  )
  // 刪 Project 本來是 `window.confirm()`：整個 app 只有這裡（與主機／身分）跳原生對話框，
  // 沒有專案全名以外的說明，也沒有 focus trap。改用跟其他刪除一致的 `ConfirmDialog`。
  const [deleteProject, setDeleteProject] = useState<{ id: string; label: string } | null>(null)
  const runs = useStore((s) => s.runs)
  // 父列收合時要看子 agent 的燈號；`runs` 已訂閱，子 agent 狀態變了這裡就會重畫。
  const lampState = useMemo(() => ({ ...useStore.getState(), runs }), [runs])
  const deleteTarget = deleteProject ? projects.find((p) => p.id === deleteProject.id) : undefined
  const deleteBlockers = deleteProject ? projectDeleteBlockers(bots, runs, deleteProject.id) : { total: 0, active: 0 }
  const [botFormFor, setBotFormFor] = useState<string | null>(null)
  const [botSheetOpen, setBotSheetOpen] = useState(false)
  const openBotSheetFor = useStore((s) => s.openBotSheetFor)
  const clearOpenBotSheet = useStore((s) => s.clearOpenBotSheet)
  const botOrder = useStore((s) => s.botOrder)
  const moveBot = useStore((s) => s.moveBot)
  const [drag, setDrag] = useState<DragState>(null)
  const shellSupported = useStore((s) => s.hostShellSupported)
  const openHostShell = useStore((s) => s.openHostShell)
  /**
   * 頂端額度卡片上被勾成「暫時停用」的身分／kind：它們底下的 bot 先從清單收起來。
   * 到了額度視窗的 reset 時刻，`quotaHide` 那邊的 timer 會把該格掃掉並通知所有訂閱者，
   * 這裡就跟著重算——bot 自己回到清單上，不用重新整理。
   */
  const disabledQuota = useDisabledQuota()
  const hiddenQuotaIds = useStore(useShallow((s) => quotaHiddenBotIds(s, disabledQuota)))
  const hiddenQuota = useMemo(() => new Set(hiddenQuotaIds), [hiddenQuotaIds])
  /** 收合起來的父 bot；記在 localStorage，重新整理後不會全部又攤開。 */
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
  /** 收合起來的專案；跟子 agent 那組同一個模式，各自一個 localStorage key。 */
  const [shutProjects, setShutProjects] = useState<Set<string>>(() => {
    try {
      const raw = localStorage.getItem('am.collapsedProjects')
      return new Set(raw ? (JSON.parse(raw) as string[]) : [])
    } catch {
      return new Set()
    }
  })
  // 新建 / 分身 / 程式化選取的 bot 要看得到：那一列可能在收合的專案裡，或在捲出畫面的地方。
  // 只在「選取的 bot 換了」時做，使用者自己捲動不會被拉回來；也不搶焦點（那是鍵盤 ↑/↓ 的事）。
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
    // 展開之後那一列才會在 DOM 裡，所以等下一個 frame 再捲。
    const id = requestAnimationFrame(() => {
      document.querySelector<HTMLElement>(`[data-bot-id="${selectedBotId}"]`)?.scrollIntoView({ block: 'nearest' })
    })
    return () => cancelAnimationFrame(id)
    // shutProjects 故意不放進依賴：使用者手動收合正在選取的專案時不該被立刻彈開。
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
  /**
   * 訊息內容的命中（`GET /api/search/messages`）。屬性比對是本地的、即時的；內容要問
   * daemon，所以 debounce 250ms，並用 `seq` 擋掉晚回來的舊請求覆蓋新結果。
   */
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
        // 舊 daemon 沒有這支：屬性搜尋照常運作，只是沒有內容命中。
        .catch(() => {
          if (hitSeq.current === mine) setHits({})
        })
    }, 250)
    return () => clearTimeout(id)
  }, [query])
  // 比對讀的是整個 store（專案、run 的 agent 標題…），所以在這裡取一次 state 就好。
  const matches = (bot: Bot) => botMatches(useStore.getState(), bot, query) || bot.id in hits
  // 只數「真的命中」的，不含為了讓子 agent 有地方掛而一起顯示的父 bot。
  const matchCount = query ? bots.filter((b) => b.team === null && matches(b)).length : bots.length
  // 只數跟 `matchCount` 同一群的（team 成員列由 TeamNodes 自己畫，不在這個計數裡），
  // 不然會出現「1 個符合（3 個含對話）」這種自相矛盾的數字。
  const hitCount = bots.filter((b) => b.team === null && b.id in hits).length

  /** 拖放：落在 overId 的上/下半 → 插到它前面 / 後面（後面 = 下一列的前面）。 */
  const dropAt = (dragId: string, overId: string, edge: 'before' | 'after') => {
    const bot = bots.find((b) => b.id === overId)
    if (!bot) return
    const ids = botsOfProject({ bots, botOrder }, bot.project_id).map((b) => b.id)
    const at = ids.indexOf(overId)
    if (at < 0) return
    const beforeId = edge === 'before' ? overId : (ids[at + 1] ?? null)
    // 「插在自己前面」就是放回原位 → 什麼都不做。不能傳 null：在 store 裡 beforeId === null
    // 是「移到最後」，那會把原地放下變成掉到清單尾巴。
    if (beforeId === dragId) return
    moveBot(dragId, beforeId)
  }

  /** Alt + ↑/↓：跟相鄰的那列交換。 */
  /**
   * Alt + ↑/↓ 換位置。只動父列：子 agent 是掛在父列底下畫的，順序由父列決定，
   * 自己搬沒有意義；而父列要跳過的也是「下一個父列」，不是夾在中間的子 agent
   * （原本把子 agent 也算進索引，往下一格常常等於原地不動）。
   */
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

  /** ↑/↓：換到相鄰的 bot，焦點跟著走（不然下一次按鍵還是從舊的那一列算）。 */
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
        {/* 縮寫是為了把寬度讓給右邊那排徽章；全名留在 title 裡。 */}
        <h1 title="Agents Manager">AG Man</h1>
        {MOCK_MODE ? <span className="mock-badge">MOCK</span> : null}
        {/* 兩列：上面 pane / RAM / 連線，下面瀏覽器分頁數對齊 pane 那欄（TabsBadge）。 */}
        <div className="head-badges">
          <PaneBadge />
          <MemBadge />
          <ConnBadge socket={socket} connected={connected} />
          <TabsBadge />
        </div>
      </div>

      {/* claude 有新版等著套用時的那一條（SPEC §6.9）。平常不佔位，只在真的有更新時出現。 */}
      <UpdateAllBanner />

      {/* 搜尋 bot：名字、專案、主機、kind、身分、模型、人設、agent 目前的標題都算數，
          因為你記得的往往不是名字（見 store 的 `botSearchText`）。 */}
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
            e.stopPropagation()
            if (e.key === 'Escape') {
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

      <div className="sidebar-scroll" role="listbox" aria-label="Bot 清單">
        {query && matchCount === 0 ? (
          <p className="hint" style={{ padding: '12px 14px' }}>
            沒有符合「{query}」的 Bot。
          </p>
        ) : null}
        {/* 斷線或重連中拿不到 state 時，空清單是「還不知道」，不是「沒有專案」——
            daemon 重啟那幾秒曾把這句誤當成引導畫面顯示出來。 */}
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
          // SPEC-team §11.4：team 成員縮排列在 Team 節點底下，不與一般 bot 混排。
          const every = botsOfProject({ bots, botOrder }, p.id).filter((b) => b.team === null)
          // 命中的子 agent 要連父 bot 一起留著（不然它沒有地方掛，會整個消失）；
          // 父 bot 命中時，它底下的子 agent 也一起顯示，當作它的脈絡。
          const hit = new Set(every.filter((b) => matches(b)).map((b) => b.id))
          const all = every.filter(
            (b) =>
              hit.has(b.id) ||
              (b.parent_bot_id ? hit.has(b.parent_bot_id) : every.some((c) => c.parent_bot_id === b.id && hit.has(c.id))),
          )
          // 身分被停用的先收起來。搜尋中不收：搜到的東西藏起來等於沒搜到（同「搜尋中一律展開」）。
          const shown = query ? all : all.filter((b) => !hiddenQuota.has(b.id))
          const hiddenCount = all.length - shown.length
          // 子 agent（bot 自己用 herdr 開的，名稱帶父 agent 前綴）縮排在父 bot 底下，不參與拖曳排序。
          const list = shown.filter((b) => !b.parent_bot_id)
          const childrenOf = (id: string) => shown.filter((b) => b.parent_bot_id === id)
          const projectSelected = selectedProjectId === p.id
          // 搜尋中一律展開：命中的 bot 藏在收合的專案裡等於沒搜到。
          const projectShut = shutProjects.has(p.id) && !query
          // 搜尋時，整個專案都沒有命中的就不佔版面——留一個空的專案標題只是雜訊。
          if (query && list.length === 0) return null
          const pDragging = pdrag?.id === p.id
          const pDropEdge = pdrag && pdrag.id !== p.id && pdrag.overId === p.id ? pdrag.edge : null
          return (
            <section
              className={`project${pDragging ? ' dragging' : ''}${pDropEdge ? ` drop-${pDropEdge}` : ''}`}
              key={p.id}
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
                onClick={() => selectProject(p.id)}
                // 抓標題列拖：整個專案（含底下的 bot）一起搬。bot 列自己也是拖曳來源，
                // 但它的 dragstart 不會冒泡到這裡（它有自己的 handler 且 pdrag 不設）。
                draggable={!query}
                // 拖曳提示掛在 `ProjectTitle` 上，不掛這裡：`title` 會被沒有自己 title 的
                // 子孫繼承，掛在 header 上時 `＋` / `⋯` 一 hover 就同時冒出原生 tooltip
                // 與自製的 `data-tip` 泡泡（兩個提示疊在一起）。
                onDragStart={(e) => {
                  e.stopPropagation()
                  e.dataTransfer.effectAllowed = 'move'
                  e.dataTransfer.setData('text/plain', `project:${p.id}`)
                  setPdrag({ id: p.id, overId: null, edge: 'before' })
                }}
                onDragEnd={() => setPdrag(null)}
              >
                {/* 收合鈕吃掉自己的 click，不然會連帶把整個專案選起來（開群組對話）。 */}
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
                    aria-expanded={false}
                    onClick={(e) => {
                      e.stopPropagation()
                      openBotSheet(p.id)
                    }}
                  >
                    ＋
                  </button>
                  {/* 刪除專案本來是一顆 `✕`，就排在「新增 Bot」的 `＋` 旁邊——建設性與
                      破壞性的動作肩並肩，而且在選取中的專案上是常駐的。跟 Team 標題列
                      同一顆 `⋯`。 */}
                  <HeadMoreMenu label={`更多動作 · ${p.label}`}>
                    {/* 開 shell 原本只長在「環境設定 → 主機」裡，要開一個 shell 得先想到它在
                        設定頁。從專案開才是常態：主機跟目錄都已經知道了（`openHostShell`
                        吃 cwd），不必再選一次。 */}
                    {shellSupported ? (
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
                    ) : null}
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
                // 整張卡片一顆可用的 bot 都不剩了，那就跟空專案長一樣：同一個 `.project-empty`
                // 容器、同一排快速新增。差別只在標題——「已隱藏」而不是「尚無」，使用者才知道
                // bot 沒有不見，只是所屬身分被停用了。停用中的那幾顆 chip 在 `QuickAddBots`
                // 裡是 disabled，不然只會再開一顆同樣沒額度的。
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
                list.map((b) => {
                  const kids = childrenOf(b.id)
                  // 搜尋中一律展開：把命中的子 agent 藏在收合的父列底下等於沒搜到。
                  const shut = kids.length > 0 && collapsed.has(b.id) && !query
                  // 收合時把子 agent 裡最要緊的燈號帶到父列：卡住的優先於在忙的，其餘不畫。
                  const kidsLamp = shut ? kidsLampOf(lampState, kids.map((c) => c.id)) : null
                  return (
                    <Fragment key={b.id}>
                      <BotRow
                        botId={b.id}
                        hit={hits[b.id]}
                        childCount={kids.length}
                        collapsed={shut}
                        kidsLamp={kidsLamp}
                        onToggleChildren={() => toggleChildren(b.id)}
                        drag={drag}
                        onDrag={setDrag}
                        onDropAt={dropAt}
                        onNudge={nudge}
                        onStep={step}
                      />
                      {shut || kids.length === 0 ? null : (
                        <div className="bot-kids" role="group" aria-label={`${b.name} 的 ${kids.length} 個子 agent`}>
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
                })
              )}
              {/* 被停用的身分收走了幾個。清單非空時當一行腳註——執行中／有未讀的現在也會被收，
                  不留一句話交代的話，使用者會以為 bot 不見了。 */}
              {!projectShut && list.length > 0 && hiddenCount > 0 ? (
                <p className="project-quota-hidden">{hiddenCount} 個 Bot 已隱藏（額度不足）</p>
              ) : null}
              {projectShut ? null : <TeamNodes projectId={p.id} />}
            </section>
          )
        })}
      </div>

      <div className="sidebar-foot">
        <div className="sidebar-foot-actions">
          <button type="button" className="btn" onClick={() => setOpen('project')}>
            新增 Project
          </button>
          <button type="button" className="btn" onClick={() => openBotSheet(selectedProjectId ?? projects[0]?.id)} disabled={projects.length === 0}>
            新增 Bot
          </button>
        </div>

        {/* 本機 shell 擺在最外層：這是「我想打個指令」最短的路徑，不該埋在設定頁裡。 */}
        {shellSupported ? (
          <button
            type="button"
            className="disclosure"
            title="在本機開一個 shell，直接下指令"
            onClick={() => void openHostShell(LOCAL_HOST)}
          >
            <TerminalIcon /> 開 shell
            <span className="disclosure-note">本機</span>
          </button>
        ) : null}

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
