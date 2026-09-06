import { useState } from 'react'
import type { HostResult } from '../api/types'
import { HOST_DEFAULTS } from '../api/types'
import { useStore } from '../store/store'
import { AttachButton } from './AttachButton'
import { ConfirmDialog } from './ConfirmDialog'
import { ToolBadges } from './Tools'

/**
 * SPEC §11.6 host management: the list of configured remote hosts (connection lamp,
 * error string, reconnect / delete) plus the "new host" form. The four §11.2 tuning
 * fields have defaults and live behind a collapsed 進階 section.
 */

/** SPEC §11.6: remote projects carry an `@<host>` badge; local ones show nothing. */
export function HostBadge({ host, connected }: { host: string; connected: boolean }) {
  if (!host || host === 'local') return null
  return (
    <span
      className={`host-badge${connected ? '' : ' down'}`}
      title={connected ? `遠端主機 ${host}（已連線）` : `遠端主機 ${host}（未連線）`}
    >
      @{host}
    </span>
  )
}

function HostRow({ name }: { name: string }) {
  const host = useStore((s) => s.hosts.find((h) => h.name === name))
  const projectCount = useStore((s) => s.projects.filter((p) => p.host === name).length)
  const busy = useStore((s) => Boolean(s.busy[`host:${name}`]))
  const reconnectHost = useStore((s) => s.reconnectHost)
  const removeHost = useStore((s) => s.removeHost)
  const [confirmDelete, setConfirmDelete] = useState(false)
  if (!host) return null

  return (
    <div className="host-row">
      <span
        className={`lamp lamp-${host.connected ? 'idle' : 'disconnected'}`}
        role="img"
        aria-label={host.connected ? '已連線' : '未連線'}
        title={host.connected ? '已連線' : (host.error ?? '未連線')}
      />
      <span className="host-main">
        <span className="host-name">
          {host.name}
          <span className="host-count">{projectCount > 0 ? `${projectCount} 個 Project` : '未使用'}</span>
        </span>
        <span className="host-ssh" title={`ssh ${host.ssh}${host.ssh_port !== 22 ? ` -p ${host.ssh_port}` : ''}`}>
          {host.ssh}
          {host.ssh_port !== 22 ? `:${host.ssh_port}` : ''} ・ {host.herdr_session}
        </span>
        {host.connected ? null : (
          <span className="host-err" title={host.error ?? ''}>
            {host.error ?? '未連線'}
          </span>
        )}
        <ToolBadges host={host.name} tools={host.tools} />
      </span>
      <span className="host-actions">
        <AttachButton command={host.attach_command} compact />
        <button
          type="button"
          className="mini-btn"
          disabled={busy}
          title="重新建立 ssh master 並 ping 遠端 herdr"
          onClick={() => void reconnectHost(name)}
        >
          {busy ? '連線中…' : '重連'}
        </button>
        <button
          type="button"
          className="icon-btn"
          title={projectCount > 0 ? '仍有 Project 使用這個主機' : '刪除主機'}
          onClick={() => setConfirmDelete(true)}
        >
          ✕
        </button>
      </span>
      {/* 本來是 `window.confirm()`——整個 app 只有這裡跳原生對話框，樣式、Escape 與
          focus trap 都跟其他刪除不一樣。 */}
      <ConfirmDialog
        open={confirmDelete}
        title="刪除主機"
        body={
          <>
            要把 <strong>{name}</strong> 從清單移除嗎？遠端的 herdr 與 agent 都不受影響，
            只是這台不再出現在 Project 的主機選項裡。
          </>
        }
        confirmLabel="刪除主機"
        danger
        onCancel={() => setConfirmDelete(false)}
        onConfirm={() => {
          setConfirmDelete(false)
          void removeHost(name)
        }}
      />
    </div>
  )
}

function NewHostForm({ onResult }: { onResult: (r: HostResult | null) => void }) {
  const addHost = useStore((s) => s.addHost)
  const [name, setName] = useState('')
  const [ssh, setSsh] = useState('')
  const [sshPort, setSshPort] = useState(String(HOST_DEFAULTS.ssh_port))
  const [session, setSession] = useState<string>(HOST_DEFAULTS.herdr_session)
  const [remotePath, setRemotePath] = useState<string>(HOST_DEFAULTS.remote_path)
  const [hookPort, setHookPort] = useState(String(HOST_DEFAULTS.hook_port))
  const [sshOpts, setSshOpts] = useState('')
  const [advanced, setAdvanced] = useState(false)
  const [busy, setBusy] = useState(false)

  const nameOk = /^[a-z][a-z0-9_-]{0,31}$/.test(name) && name !== 'local'
  const ok = nameOk && ssh.trim().length > 0

  return (
    <form
      className="form"
      onSubmit={(e) => {
        e.preventDefault()
        if (!ok || busy) return
        setBusy(true)
        onResult(null)
        void addHost({
          name,
          ssh: ssh.trim(),
          ssh_port: Number(sshPort) || HOST_DEFAULTS.ssh_port,
          herdr_session: session.trim() || HOST_DEFAULTS.herdr_session,
          remote_path: remotePath.trim(),
          hook_port: Number(hookPort) || HOST_DEFAULTS.hook_port,
          ...(sshOpts.trim() ? { ssh_opts: sshOpts.trim().split(/\s+/) } : {}),
        }).then((res) => {
          setBusy(false)
          onResult(res)
          if (res) {
            setName('')
            setSsh('')
          }
        })
      }}
    >
      <label className="field">
        <span>名稱（唯一識別，用於 Project 的 host）</span>
        <input
          type="text"
          value={name}
          placeholder="m4p"
          spellCheck={false}
          onChange={(e) => setName(e.target.value.toLowerCase())}
        />
        {name && !nameOk ? (
          <span className="hint">必須符合 [a-z][a-z0-9_-]&#123;0,31&#125;，且不可為 local</span>
        ) : null}
      </label>
      <label className="field">
        <span>ssh 目標（可用 ssh_config 別名）</span>
        <input
          type="text"
          value={ssh}
          placeholder="m4p@100.112.229.82"
          spellCheck={false}
          onChange={(e) => setSsh(e.target.value)}
        />
      </label>

      <button type="button" className="disclosure sub" aria-expanded={advanced} onClick={() => setAdvanced(!advanced)}>
        <span className="chev">{advanced ? '▼' : '▶'}</span> 進階（都有預設值）
      </button>
      {advanced ? (
        <>
          <label className="field">
            <span>ssh_port</span>
            <input type="text" value={sshPort} spellCheck={false} onChange={(e) => setSshPort(e.target.value)} />
          </label>
          <label className="field">
            <span>herdr_session（遠端 named session）</span>
            <input type="text" value={session} spellCheck={false} onChange={(e) => setSession(e.target.value)} />
          </label>
          <label className="field">
            <span>remote_path（非互動 ssh shell 要前置的 PATH）</span>
            <input
              type="text"
              value={remotePath}
              spellCheck={false}
              onChange={(e) => setRemotePath(e.target.value)}
            />
          </label>
          <label className="field">
            <span>hook_port（遠端 127.0.0.1 上的反向轉發埠）</span>
            <input type="text" value={hookPort} spellCheck={false} onChange={(e) => setHookPort(e.target.value)} />
          </label>
          <label className="field">
            <span>ssh_opts（額外 ssh 參數，以空白分隔）</span>
            <input
              type="text"
              value={sshOpts}
              placeholder="-i ~/.ssh/id_ed25519"
              spellCheck={false}
              onChange={(e) => setSshOpts(e.target.value)}
            />
          </label>
        </>
      ) : null}

      <div className="form-actions">
        <span className="hint host-form-hint">只用現有 ssh key（BatchMode），不會詢問密碼</span>
        <button type="submit" className="btn primary" disabled={!ok || busy}>
          {busy ? '連線中…' : '新增並連線'}
        </button>
      </div>
    </form>
  )
}

/** The daemon's own machine: connection lamp, attach command, tool badges (v4.0). */
function LocalHostRow() {
  const connected = useStore((s) => s.connected)
  const tools = useStore((s) => s.localTools)
  const attach = useStore((s) => s.attachCommand)
  const projectCount = useStore((s) => s.projects.filter((p) => p.host === 'local').length)
  return (
    <div className="host-row local">
      <span className={`lamp lamp-${connected ? 'idle' : 'disconnected'}`} role="img" aria-label={connected ? 'herdr 已連線' : 'herdr 中斷'} title={connected ? 'herdr 已連線' : 'herdr 中斷'} />
      <span className="host-main">
        <span className="host-name">
          本機
          <span className="host-count">{projectCount > 0 ? `${projectCount} 個 Project` : '未使用'}</span>
        </span>
        <span className="host-ssh">{attach}</span>
        <ToolBadges host="local" tools={tools} />
      </span>
      <span className="host-actions">
        <AttachButton command={attach} compact />
      </span>
    </div>
  )
}

export function HostsPanel() {
  const hosts = useStore((s) => s.hosts)
  const [result, setResult] = useState<HostResult | null>(null)

  return (
    <div className="hosts-panel">
      <div className="host-list">
        <LocalHostRow />
      </div>
      {hosts.length === 0 ? (
        <p className="hint hosts-empty">尚未設定遠端主機。Project 預設都在本機。</p>
      ) : (
        <div className="host-list">
          {hosts.map((h) => (
            <HostRow key={h.name} name={h.name} />
          ))}
        </div>
      )}
      {result ? (
        <div className={`host-result ${result.connected ? 'ok' : 'err'}`} role="status">
          {result.connected
            ? `✓ ${result.name}：ssh master 與遠端 herdr 都就緒`
            : `✕ ${result.name}：${result.error ?? '連線失敗'}`}
        </div>
      ) : null}
      <NewHostForm onResult={setResult} />
    </div>
  )
}
