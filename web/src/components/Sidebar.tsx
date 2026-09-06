import { useEffect, useMemo, useRef, useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import { MOCK_MODE } from '../api'
import type { BotKind } from '../api/types'
import { BOT_KINDS } from '../api/types'
import {
  adjacentBotId,
  anchorOf,
  attachCommandOf,
  botLamp,
  botQuotaWarning,
  botsOfProject,
  identitiesOfHost,
  projectHostName,
  toolsOfHost,
  useStore,
} from '../store/store'
import type { SocketStatus } from '../store/store'
import { CloneIcon, GearIcon, PlayIcon, TrashIcon } from './Icons'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'
import { ConfirmDialog } from './ConfirmDialog'
import { HeadMoreMenu } from './HeadMoreMenu'
import { DirPicker } from './DirPicker'
import { IdentitiesPanel, IdentityBadge } from './IdentitiesPanel'
import { Modal } from './Modal'
import { IdentityOptions, PersonaField, PersonaMark } from './BotSettingsPanel'
import { HostBadge, HostsPanel } from './HostsPanel'
import { AttachButton } from './AttachButton'
import { BotNameField } from './BotNameField'
import { MemBadge } from './MemBadge'
import { ModelTag } from './ModelTag'
import { KIND_LABEL, KindDisplayToggle, KindTag } from './KindTag'
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
  drag,
  onDrag,
  onDropAt,
  onNudge,
  onStep,
}: {
  botId: string
  drag: DragState
  onDrag: (next: DragState) => void
  onDropAt: (dragId: string, overId: string, edge: 'before' | 'after') => void
  onNudge: (botId: string, dir: -1 | 1) => void
  onStep: (botId: string, dir: -1 | 1) => void
}) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId))
  const run = useStore((s) => s.runs[botId] ?? null)
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
  const selected = useStore((s) => s.selectedBotId === botId)
  const busyStart = useStore((s) => Boolean(s.busy[`start:${botId}`]))
  const selectBot = useStore((s) => s.selectBot)
  const startBot = useStore((s) => s.startBot)
  const openSettings = useStore((s) => s.openSettings)
  const cloneBot = useStore((s) => s.cloneBot)
  const busyClone = useStore((s) => Boolean(s.busy[`clone:${botId}`]))
  const removeBot = useStore((s) => s.removeBot)
  const [deleteOpen, setDeleteOpen] = useState(false)
  const agentTitle = useStore((s) => {
    const r = s.runs[botId]
    const t = r?.agent_title?.trim()
    if (!t) return ''
    const l = botLamp(s, botId)
    return l === 'working' || l === 'idle' ? t : ''
  })

  if (!bot) return null
  const active = run !== null && run.state !== 'stopped' && run.state !== 'exited'
  // 標題只在選取中的那一列展開成一行——一次只有一列，清單的掃讀節奏不會被打亂。
  const showTitle = Boolean(agentTitle) && selected

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
      className={`bot-row${selected ? ' selected' : ''}${dragging ? ' dragging' : ''}${
        dropEdge ? ` drop-${dropEdge}` : ''
      }${deleteOpen ? ' confirming' : ''}${quotaWarning ? ' quota-critical' : ''}`}
      role="option"
      aria-selected={selected}
      data-bot-id={botId}
      tabIndex={0}
      draggable
      onClick={() => selectBot(botId)}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          selectBot(botId)
        }
        if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return
        const dir = e.key === 'ArrowUp' ? -1 : 1
        // 鍵盤也要能排序：Alt + ↑/↓（拖曳不是每個人都能用）。
        if (e.altKey) {
          e.preventDefault()
          onNudge(botId, dir)
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
      <StatusLamp lamp={lamp} title={`${bot.name}：${LAMP_LABEL[lamp]}${agentTitle && !showTitle ? ` · ${agentTitle}` : ''}`} />
      <span className="bot-main">
        {/* 選取中的那一列，名字點下去就改名（未選取的第一下還是「開啟這個 bot」）。 */}
        <BotNameField botId={botId} name={bot.name} variant="row" armed={selected}>
          <PersonaMark persona={bot.persona} />
          {/* agent 自己的標題不再跟名字擠同一行——那樣兩邊各剩六個字
              （`C0-畫面修改者 資料夾…`）。選取中的那一列給它自己一行（見下面），
              其餘的列名字獨佔第一行，標題在整列的 tooltip 裡。 */}
          {showTitle ? null : <span className={`bot-state ${lamp}`}>{LAMP_LABEL[lamp]}</span>}
        </BotNameField>
        <span className="bot-sub">
          <KindTag kind={bot.kind} />
          {/* 身份（cc0 / cc1…）一定要標，同一個 CLI 兩個帳號才分得出來。 */}
          <IdentityBadge name={bot.identity} showDefault />
          {quotaWarning ? (
            // 額度 critical：警語取代模型標籤（側欄窄，優先顯示這個）；文字撐不下就截斷，完整內容看 title。
            <span
              className="bot-quota-warn"
              title={`${KIND_LABEL[bot.kind]}${bot.identity ? ` · ${bot.identity}` : ''} ${quotaWarning.window} 額度剩 ${quotaWarning.pct}%，快用完了`}
            >
              ⚠ 額度剩 {quotaWarning.pct}%
            </span>
          ) : (
            <ModelTag botId={botId} />
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
      <span className="bot-actions" onClick={(e) => e.stopPropagation()}>
        {/* 這一列的選單就兩個鍵，上下疊：開同類分身 / 設定。 */}
        <span className="bot-menu">
          <button
            type="button"
            className="icon-btn menu-btn icon-tip"
            disabled={busyClone}
            aria-label={`開 ${bot.name} 的同類分身並啟動（同 kind、模型、身份、人設）`}
            data-tip={`開同類分身並啟動 · ${bot.name}`}
            onClick={() => void cloneBot(botId)}
          >
            <CloneIcon />
          </button>
          <button
            type="button"
            className="icon-btn menu-btn gear icon-tip"
            aria-label={`設定 ${bot.name}（模型、身份、autostart…）`}
            data-tip={`設定 · ${bot.name}`}
            onClick={(e) => openSettings(botId, anchorOf(e.currentTarget))}
          >
            <GearIcon />
          </button>
        </span>
        <button
          type="button"
          className="icon-btn bot-delete-btn icon-tip"
          aria-label={`刪除 ${bot.name}`}
          data-tip={`刪除 · ${bot.name}`}
          onClick={() => setDeleteOpen(true)}
        >
          <TrashIcon />
        </button>
        {active ? null : (
          <button
            type="button"
            className="icon-btn bot-run-btn start icon-tip"
            disabled={busyStart}
            aria-label={`啟動 ${bot.name}`}
            data-tip={`啟動 · ${bot.name}`}
            onClick={() => void startBot(botId)}
          >
            <PlayIcon />
          </button>
        )}
      </span>

      <ConfirmDialog
        open={deleteOpen}
        title="刪除 Bot"
        body={
          <>
            確定刪除 <strong>{bot.name}</strong>？會停止並關閉它的終端 pane，設定從 config.toml 移除；對話紀錄會保留。
          </>
        }
        confirmLabel="刪除"
        danger
        width={340}
        onCancel={() => setDeleteOpen(false)}
        onConfirm={() => {
          setDeleteOpen(false)
          void removeBot(botId)
        }}
      />
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
      <ApiModelFields kind={kind} host={host} model={model} onModel={setModel} effort={effort} onEffort={setEffort} fast={fast} onFast={setFast} />
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
function ProjectTitle({ projectId, label, host, path, hostUp }: { projectId: string; label: string; host: string; path: string; hostUp: boolean }) {
  const selected = useStore((s) => s.selectedProjectId === projectId)
  const unread = useStore((s) => s.groupUnread[projectId] ?? 0)
  const selectProject = useStore((s) => s.selectProject)
  return (
    <button
      type="button"
      className={`project-label-btn${selected ? ' selected' : ''}`}
      title={`開啟「${label}」的群組聊天（@bot 或 @all 對多個 Bot 發言）\n${path}`}
      aria-pressed={selected}
      onClick={(e) => {
        e.stopPropagation()
        selectProject(projectId)
      }}
    >
      <span className="project-group-icon" aria-hidden="true">
        ⌗
      </span>
      <span className="project-label">{label}</span>
      {unread > 0 ? (
        <span className="unread-badge" title={`${unread} 則未讀的群組回覆`}>
          {unread > 99 ? '99+' : unread}
        </span>
      ) : null}
      <HostBadge host={host} connected={hostUp} />
      <span className="project-path" title={path}>
        {shortPath(path, 36)}
      </span>
    </button>
  )
}

/** Hover-only「在終端開啟」for a project head (its host's attach command). */
function ProjectAttach({ projectId }: { projectId: string }) {
  const command = useStore((s) => attachCommandOf(s, projectId))
  return <AttachButton command={command} compact />
}

export function Sidebar() {
  const projects = useStore((s) => s.projects)
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
  const [botFormFor, setBotFormFor] = useState<string | null>(null)
  const [botSheetOpen, setBotSheetOpen] = useState(false)
  const openBotSheetFor = useStore((s) => s.openBotSheetFor)
  const clearOpenBotSheet = useStore((s) => s.clearOpenBotSheet)
  const botOrder = useStore((s) => s.botOrder)
  const moveBot = useStore((s) => s.moveBot)
  const [drag, setDrag] = useState<DragState>(null)

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
  const nudge = (botId: string, dir: -1 | 1) => {
    const bot = bots.find((b) => b.id === botId)
    if (!bot) return
    const ids = botsOfProject({ bots, botOrder }, bot.project_id).map((b) => b.id)
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
        <PaneBadge />
        <MemBadge />
        <ConnBadge socket={socket} connected={connected} />
      </div>

      <div className="sidebar-scroll" role="listbox" aria-label="Bot 清單">
        {projects.length === 0 ? (
          <p className="hint" style={{ padding: '12px 14px' }}>
            尚未設定任何 Project。請用下方的「新增 Project」開始。
          </p>
        ) : null}
        {projects.map((p) => {
          // SPEC-team §11.4：team 成員縮排列在 Team 節點底下，不與一般 bot 混排。
          const list = botsOfProject({ bots, botOrder }, p.id).filter((b) => b.team === null)
          const projectSelected = selectedProjectId === p.id
          return (
            <section className="project" key={p.id}>
              <header
                className={`project-head${projectSelected ? ' selected' : ''}`}
                onClick={() => selectProject(p.id)}
              >
                <ProjectTitle projectId={p.id} label={p.label} host={p.host} path={p.path} hostUp={hostUp(p.host)} />
                <span className="project-head-actions">
                  <ProjectAttach projectId={p.id} />
                  <button
                    type="button"
                    className="icon-btn add icon-tip"
                    title={`在「${p.label}」新增 Bot`}
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
              {list.length === 0 ? (
                <div className="project-empty">
                  <span className="project-empty-title">此專案尚無 Bot</span>
                  <button type="button" className="btn primary empty-add-btn" onClick={() => openBotSheet(p.id)}>
                    新增 Bot
                  </button>
                </div>
              ) : (
                list.map((b) => (
                  <BotRow key={b.id} botId={b.id} drag={drag} onDrag={setDrag} onDropAt={dropAt} onNudge={nudge} onStep={step} />
                ))
              )}
              <TeamNodes projectId={p.id} />
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
            要把 <strong>{deleteProject?.label}</strong> 從清單移除嗎？磁碟上的目錄與其中的檔案都不會動，
            這個 Project 底下的 Bot 需要先停止。
          </>
        }
        confirmLabel="刪除 Project"
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
