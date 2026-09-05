import { useEffect, useRef, useState } from 'react'
import { MOCK_MODE } from '../api'
import type { BotKind } from '../api/types'
import { BOT_KINDS } from '../api/types'
import { attachCommandOf, botLamp, projectHostName, toolsOfHost, useStore } from '../store/store'
import type { SocketStatus } from '../store/store'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'
import { DirPicker } from './DirPicker'
import { IdentitiesPanel, IdentityBadge } from './IdentitiesPanel'
import { ModelField, IdentityOptions, PersonaField, PersonaMark } from './BotSettingsPanel'
import { HostBadge, HostsPanel } from './HostsPanel'
import { AttachButton } from './AttachButton'
import { KindDisplayToggle, KindTag } from './KindTag'
import { ApiModelFields } from './ModelPicker'
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

/** Keep the tail of a long path readable without relying on RTL text tricks. */
function shortPath(path: string, max = 30): string {
  return path.length <= max ? path : `…${path.slice(-(max - 1))}`
}

function BotRow({ botId }: { botId: string }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId))
  const run = useStore((s) => s.runs[botId] ?? null)
  const lamp = useStore((s) => botLamp(s, botId))
  const selected = useStore((s) => s.selectedBotId === botId)
  const busyStart = useStore((s) => Boolean(s.busy[`start:${botId}`]))
  const busyStop = useStore((s) => Boolean(s.busy[`stop:${botId}`]))
  const selectBot = useStore((s) => s.selectBot)
  const startBot = useStore((s) => s.startBot)
  const stopBot = useStore((s) => s.stopBot)
  const openSettings = useStore((s) => s.openSettings)
  const [menuOpen, setMenuOpen] = useState(false)
  const menuRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!menuOpen) return
    const onDoc = (e: MouseEvent) => {
      if (!menuRef.current?.contains(e.target as Node)) setMenuOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setMenuOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDoc)
      document.removeEventListener('keydown', onKey)
    }
  }, [menuOpen])

  if (!bot) return null
  const active = run !== null && run.state !== 'stopped' && run.state !== 'exited'

  return (
    <div
      className={`bot-row${selected ? ' selected' : ''}${menuOpen ? ' menu-open' : ''}`}
      role="option"
      aria-selected={selected}
      tabIndex={0}
      onClick={() => selectBot(botId)}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          selectBot(botId)
        }
      }}
    >
      <StatusLamp lamp={lamp} title={`${bot.name}：${LAMP_LABEL[lamp]}`} />
      <span className="bot-main">
        <span className="bot-name">
          {bot.name}
          <PersonaMark persona={bot.persona} />
          {/* idle / offline are hidden by CSS — the lamp already says so. */}
          <span className={`bot-state ${lamp}`}>{LAMP_LABEL[lamp]}</span>
        </span>
        <span className="bot-sub">
          <KindTag kind={bot.kind} />
          {bot.model ? (
            <span className="model-tag" title={`模型：${bot.model}`}>
              {bot.model}
            </span>
          ) : (
            <IdentityBadge name={bot.identity} />
          )}
        </span>
      </span>
      <span className="bot-actions" onClick={(e) => e.stopPropagation()}>
        <button
          type="button"
          className="icon-btn gear icon-tip"
          title={`設定 ${bot.name}（模型、身份、autostart…）`}
          aria-label={`設定 ${bot.name}`}
          data-tip={`設定 · ${bot.name}`}
          onClick={() => openSettings(botId)}
        >
          ⚙
        </button>
        <div className="bot-menu" ref={menuRef}>
          <button
            type="button"
            className="icon-btn bot-menu-btn icon-tip"
            aria-label={`${bot.name} 的操作選單`}
            aria-haspopup="menu"
            aria-expanded={menuOpen}
            title={`${active ? '停止' : '啟動'} ${bot.name}`}
            data-tip={`${active ? '停止' : '啟動'} · ${bot.name}`}
            onClick={() => setMenuOpen((v) => !v)}
          >
            ⋯
          </button>
          {menuOpen ? (
            <div className="bot-menu-pop" role="menu">
              {active ? (
                <button
                  type="button"
                  role="menuitem"
                  className="bot-menu-item danger"
                  disabled={busyStop}
                  onClick={() => {
                    setMenuOpen(false)
                    void stopBot(botId)
                  }}
                >
                  停止
                </button>
              ) : (
                <button
                  type="button"
                  role="menuitem"
                  className="bot-menu-item"
                  disabled={busyStart}
                  onClick={() => {
                    setMenuOpen(false)
                    void startBot(botId)
                  }}
                >
                  啟動
                </button>
              )}
              <button
                type="button"
                role="menuitem"
                className="bot-menu-item"
                onClick={() => {
                  setMenuOpen(false)
                  openSettings(botId)
                }}
              >
                設定
              </button>
            </div>
          ) : null}
        </div>
      </span>
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
          effort: kind === 'claude' ? null : effort,
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
      {kind === 'claude' ? (
        <ModelField kind={kind} value={model} onChange={setModel} />
      ) : (
        <ApiModelFields kind={kind} host={host} model={model} onModel={setModel} effort={effort} onEffort={setEffort} fast={fast} onFast={setFast} />
      )}
      <IdentityOptions kind={kind} value={identity} onChange={setIdentity} />
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
  const identityCount = useStore((s) => s.identities.length)
  const [botFormFor, setBotFormFor] = useState<string | null>(null)
  const [botSheetOpen, setBotSheetOpen] = useState(false)
  const openBotSheetFor = useStore((s) => s.openBotSheetFor)
  const clearOpenBotSheet = useStore((s) => s.clearOpenBotSheet)

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

  if (botSheetOpen) {
    return (
      <>
        <div className="sidebar-head">
          <h1>Agents Manager</h1>
          {MOCK_MODE ? <span className="mock-badge">MOCK</span> : null}
          <ConnBadge socket={socket} connected={connected} />
        </div>
        <div className="new-bot-sheet">
          <div className="sheet-head">
            <button type="button" className="icon-btn" aria-label="返回" title="返回清單" onClick={closeBotSheet}>
              ←
            </button>
            <strong title={botSheetProject?.path}>
              新增 Bot · {botSheetLabel}
            </strong>
          </div>
          <div className="sheet-body inline-form">
            <NewBotForm
              key={botFormFor ?? 'pick'}
              initialProjectId={botFormFor ?? undefined}
              onDone={closeBotSheet}
            />
          </div>
        </div>
      </>
    )
  }

  if (open === 'project') {
    return (
      <>
        <div className="sidebar-head">
          <h1>Agents Manager</h1>
          {MOCK_MODE ? <span className="mock-badge">MOCK</span> : null}
          <ConnBadge socket={socket} connected={connected} />
        </div>
        <div className="new-project-sheet">
          <div className="sheet-head">
            <button type="button" className="icon-btn" aria-label="返回" title="返回清單" onClick={() => setOpen(null)}>
              ←
            </button>
            <strong>新增 Project</strong>
          </div>
          <div className="sheet-body">
            <NewProjectForm onDone={() => setOpen(null)} />
          </div>
        </div>
      </>
    )
  }

  return (
    <>
      <div className="sidebar-head">
        <h1>Agents Manager</h1>
        {MOCK_MODE ? <span className="mock-badge">MOCK</span> : null}
        <ConnBadge socket={socket} connected={connected} />
      </div>

      <div className="sidebar-scroll" role="listbox" aria-label="Bot 清單">
        {projects.length === 0 ? (
          <p className="hint" style={{ padding: '12px 14px' }}>
            尚未設定任何 Project。請用下方的「新增 Project」開始。
          </p>
        ) : null}
        {projects.map((p) => {
          const list = bots.filter((b) => b.project_id === p.id)
          const projectSelected = selectedProjectId === p.id
          return (
            <section className="project" key={p.id}>
              <header
                className={`project-head${projectSelected ? ' selected' : ''}`}
                onClick={() => selectProject(p.id)}
              >
                <ProjectTitle projectId={p.id} label={p.label} host={p.host} path={p.path} hostUp={hostUp(p.host)} />
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
                <button
                  type="button"
                  className="icon-btn icon-tip"
                  title={`刪除專案「${p.label}」（所有 Bot 需先停止）`}
                  aria-label={`刪除專案 ${p.label}`}
                  data-tip={`刪除專案 · ${p.label}`}
                  onClick={(e) => {
                    e.stopPropagation()
                    if (confirm(`刪除 Project「${p.label}」？（不會刪除目錄）`)) void removeProject(p.id)
                  }}
                >
                  ✕
                </button>
              </header>
              {list.length === 0 ? (
                <div className="project-empty">
                  <span className="project-empty-title">此專案尚無 Bot</span>
                  <button type="button" className="btn primary empty-add-btn" onClick={() => openBotSheet(p.id)}>
                    新增 Bot
                  </button>
                </div>
              ) : (
                list.map((b) => <BotRow key={b.id} botId={b.id} />)
              )}
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
          aria-expanded={open === 'env'}
          onClick={() => setOpen(open === 'env' ? null : 'env')}
        >
          <span className="chev">{open === 'env' ? '▼' : '▶'}</span> 環境設定
          <span className="disclosure-note">
            {hosts.length === 0 ? '本機' : `本機 + ${hosts.length}`}
            {hostsDown > 0 ? ` ・ ${hostsDown} 未連線` : ''}
            {` ・ 身分 ${identityCount}`}
          </span>
        </button>
        {open === 'env' ? (
          <div className="env-panel">
            <HostsPanel />
            <IdentitiesPanel />
            <KindDisplayToggle />
          </div>
        ) : null}
      </div>
    </>
  )
}
