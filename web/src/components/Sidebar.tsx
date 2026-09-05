import { useState } from 'react'
import { MOCK_MODE } from '../api'
import type { BotKind } from '../api/types'
import { botLamp, useStore } from '../store/store'
import type { SocketStatus } from '../store/store'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'
import { DirPicker } from './DirPicker'

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

  if (!bot) return null
  const active = run !== null && run.state !== 'stopped' && run.state !== 'exited'

  return (
    <div
      className={`bot-row${selected ? ' selected' : ''}`}
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
        <span className="bot-name">{bot.name}</span>
        <span className="bot-sub">
          <span className={`kind-tag ${bot.kind}`}>{bot.kind}</span>
          <span>{LAMP_LABEL[lamp]}</span>
        </span>
      </span>
      <span className="bot-actions" onClick={(e) => e.stopPropagation()}>
        {active ? (
          <button
            type="button"
            className="mini-btn danger"
            disabled={busyStop}
            onClick={() => void stopBot(botId)}
            title="stop：ctrl+c ×2，必要時關閉 pane"
          >
            停止
          </button>
        ) : (
          <button
            type="button"
            className="mini-btn primary"
            disabled={busyStart}
            onClick={() => void startBot(botId)}
            title="start：建立 pane 並啟動 agent"
          >
            啟動
          </button>
        )}
      </span>
    </div>
  )
}

function NewProjectForm({ onDone }: { onDone: () => void }) {
  const addProject = useStore((s) => s.addProject)
  const [path, setPath] = useState('')
  const [label, setLabel] = useState('')
  const [busy, setBusy] = useState(false)
  const [browsing, setBrowsing] = useState(false)

  if (browsing) {
    return (
      <DirPicker
        initial={path}
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
        void addProject({ path: path.trim(), label: label.trim() }).then((ok) => {
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
        <span>目錄路徑</span>
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

function NewBotForm({ onDone }: { onDone: () => void }) {
  const projects = useStore((s) => s.projects)
  const addBot = useStore((s) => s.addBot)
  const [projectId, setProjectId] = useState(projects[0]?.id ?? '')
  const [name, setName] = useState('')
  const [kind, setKind] = useState<BotKind>('claude')
  const [args, setArgs] = useState('')
  const [autostart, setAutostart] = useState(false)
  const [autoApprove, setAutoApprove] = useState(true)
  const [busy, setBusy] = useState(false)

  const pid = projectId || projects[0]?.id || ''
  const nameOk = /^[a-z][a-z0-9_-]{0,31}$/.test(name)

  if (projects.length === 0) {
    return <p className="hint">請先新增一個 Project。</p>
  }

  return (
    <form
      className="form"
      onSubmit={(e) => {
        e.preventDefault()
        if (!nameOk || !pid || busy) return
        setBusy(true)
        void addBot(pid, {
          name,
          kind,
          args: args.trim() ? args.trim().split(/\s+/) : [],
          autostart,
          auto_approve: autoApprove,
        }).then((ok) => {
          setBusy(false)
          if (ok) {
            setName('')
            setArgs('')
            onDone()
          }
        })
      }}
    >
      <label className="field">
        <span>Project</span>
        <select value={pid} onChange={(e) => setProjectId(e.target.value)}>
          {projects.map((p) => (
            <option key={p.id} value={p.id}>
              {p.label}
            </option>
          ))}
        </select>
      </label>
      <label className="field">
        <span>名稱（herdr agent name，全域唯一）</span>
        <input
          type="text"
          value={name}
          placeholder="foo-claude"
          spellCheck={false}
          onChange={(e) => setName(e.target.value.toLowerCase())}
        />
        {name && !nameOk ? <span className="hint">必須符合 [a-z][a-z0-9_-]&#123;0,31&#125;</span> : null}
      </label>
      <label className="field">
        <span>kind</span>
        <select value={kind} onChange={(e) => setKind(e.target.value as BotKind)}>
          <option value="claude">claude</option>
          <option value="codex">codex</option>
        </select>
      </label>
      <label className="field">
        <span>args（以空白分隔）</span>
        <input
          type="text"
          value={args}
          placeholder="--model opus"
          spellCheck={false}
          onChange={(e) => setArgs(e.target.value)}
        />
      </label>
      <label className="field row">
        <input type="checkbox" checked={autostart} onChange={(e) => setAutostart(e.target.checked)} />
        <span>autostart（daemon 啟動時自動執行）</span>
      </label>
      <label className="field row">
        <input type="checkbox" checked={autoApprove} onChange={(e) => setAutoApprove(e.target.checked)} />
        <span>
          自動核准全部權限（claude <code>--dangerously-skip-permissions</code> / codex <code>--yolo</code>）
        </span>
      </label>
      <div className="form-actions">
        <button type="button" className="btn" onClick={onDone}>
          取消
        </button>
        <button type="submit" className="btn primary" disabled={!nameOk || busy}>
          新增
        </button>
      </div>
    </form>
  )
}

export function Sidebar() {
  const projects = useStore((s) => s.projects)
  const bots = useStore((s) => s.bots)
  const socket = useStore((s) => s.socket)
  const connected = useStore((s) => s.connected)
  const removeProject = useStore((s) => s.removeProject)
  const [open, setOpen] = useState<'project' | 'bot' | null>(null)

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
          return (
            <section className="project" key={p.id}>
              <header className="project-head">
                <span className="project-label">{p.label}</span>
                <span className="project-path" title={p.path}>
                  {shortPath(p.path)}
                </span>
                <button
                  type="button"
                  className="icon-btn"
                  title="刪除 Project（所有 Bot 需先停止）"
                  onClick={() => {
                    if (confirm(`刪除 Project「${p.label}」？（不會刪除目錄）`)) void removeProject(p.id)
                  }}
                >
                  ✕
                </button>
              </header>
              {list.length === 0 ? (
                <p className="hint" style={{ padding: '0 14px 6px' }}>
                  這個 Project 還沒有 Bot。
                </p>
              ) : (
                list.map((b) => <BotRow key={b.id} botId={b.id} />)
              )}
            </section>
          )
        })}
      </div>

      <div className="sidebar-foot">
        <button
          type="button"
          className="disclosure"
          aria-expanded={open === 'project'}
          onClick={() => setOpen(open === 'project' ? null : 'project')}
        >
          <span className="chev">{open === 'project' ? '▼' : '▶'}</span> 新增 Project
        </button>
        {open === 'project' ? <NewProjectForm onDone={() => setOpen(null)} /> : null}

        <button
          type="button"
          className="disclosure"
          aria-expanded={open === 'bot'}
          onClick={() => setOpen(open === 'bot' ? null : 'bot')}
        >
          <span className="chev">{open === 'bot' ? '▼' : '▶'}</span> 新增 Bot
        </button>
        {open === 'bot' ? <NewBotForm onDone={() => setOpen(null)} /> : null}
      </div>
    </>
  )
}
