import { useEffect, useRef, useState } from 'react'
import * as api from '../api'
import type { CloseNeedsConfirm } from '../api'
import type { HerdrVersion, HostBaseline, HostResult, HostShell, RemoteCargoSettings } from '../api/types'
import { herdrVersionView } from '../lib/herdrVersion'
import { hostBaselineView } from '../lib/hostBaseline'
import { baselineWarnWhy } from '../lib/toolInstall'
import { ApiError, HOST_DEFAULTS } from '../api/types'
import { nestableBusy } from '../lib/nestableBusy'
import { DEFAULT_REMOTE_ROOT, remoteCargoProblems, remoteCargoUnsaved, toRemoteCargoInput } from '../lib/remoteCargoForm'
import { portProblem, sessionProblem, sshTargetProblem } from '../lib/hostForm'
import { useUnsavedGuard } from '../hooks/useUnsavedGuard'
import { useStore } from '../store/store'
import { useT } from '../lib/i18n'
import { AttachButton } from './AttachButton'
import { ConfirmDialog } from './ConfirmDialog'
import { GhHostStatus } from './GhAuth'
import { HostBaselineApply } from './HostBaselineApply'
import { HostBaselineRestore } from './HostBaselineRestore'
import { ServicePaneNote } from './ServicePaneNote'
import { ToolInstallGuide } from './ToolInstallGuide'
import { ToolBadges } from './Tools'
import './hostsPanel.css'
import { useUserTimeZone } from '../lib/userTime'

export function HostBadge({ host, connected }: { host: string; connected: boolean }) {
  const t = useT()
  if (!host || host === 'local') return null
  return (
    <span
      className={`host-badge${connected ? '' : ' down'}`}
      title={connected ? t('遠端主機 {host}（已連線）', { host }) : t('遠端主機 {host}（未連線）', { host })}
    >
      @{host}
      {connected ? null : t(' 離線')}
    </span>
  )
}

function closeEnclosingPopup(from: HTMLElement | null) {
  if (!from?.closest('.modal-backdrop')) return
  from.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
}

function OpenShellButton({ host, connected }: { host: string; connected: boolean }) {
  const t = useT()
  const busy = useStore((s) => Boolean(s.busy[`shell:${host}`]))
  const openHostShell = useStore((s) => s.openHostShell)
  return (
    <button
      type="button"
      className="mini-btn"
      disabled={busy || !connected}
      aria-label={host === 'local' ? t('在本機開 shell') : t('在 {host} 開 shell', { host })}
      title={connected ? t('在這台主機開一個 shell，直接下指令') : t('主機未連線，開不了 shell')}
      onClick={(e) => {
        const btn = e.currentTarget
        void openHostShell(host).then((ok) => {
          if (ok) closeEnclosingPopup(btn)
        })
      }}
    >
      {busy ? t('開啟中…') : t('開 shell')}
    </button>
  )
}

function HostShellList({ host, connected }: { host: string; connected: boolean }) {
  const t = useT()
  const current = useStore((s) => s.shellView)
  const viewHostShell = useStore((s) => s.viewHostShell)
  const endHostShell = useStore((s) => s.endHostShell)
  const busy = useStore((s) => s.busy)
  const opening = Boolean(busy[`shell:${host}`])
  const [shells, setShells] = useState<HostShell[]>([])
  const [tick, setTick] = useState(0)
  const [needs, setNeeds] = useState<{ paneId: string; why: CloseNeedsConfirm } | null>(null)
  const enabled = connected

  useEffect(() => {
    if (!enabled) return
    let alive = true
    void api.fetchHostShells(host).then(
      (list) => {
        if (alive) setShells(list)
      },
      () => {
      },
    )
    return () => {
      alive = false
    }
  }, [enabled, host, tick, current, opening])

  if (!enabled || shells.length === 0) return null
  return (
    <span className="host-shells">
      {shells.map((sh) => {
        const active = current?.host === host && current.paneId === sh.pane_id
        const ending = Boolean(busy[`shell:${host}:${sh.pane_id}`])
        return (
          <span key={sh.pane_id} className={`host-shell${active ? ' active' : ''}`}>
            <button
              type="button"
              className="host-shell-open mono"
              title={t('回到這個 shell（pane {pane}）\n{cwd}', { pane: sh.pane_id, cwd: sh.cwd })}
              onClick={(e) => {
                const btn = e.currentTarget
                viewHostShell(sh)
                closeEnclosingPopup(btn)
              }}
            >
              ❯ {sh.cwd.split('/').filter(Boolean).slice(-2).join('/') || sh.cwd}
            </button>
            <button
              type="button"
              className="icon-btn"
              disabled={ending}
              aria-label={t('結束 shell {cwd}（pane {pane}）', { cwd: sh.cwd, pane: sh.pane_id })}
              title={t('結束 shell {cwd}（pane {pane}）', { cwd: sh.cwd, pane: sh.pane_id })}
              onClick={() =>
                void endHostShell(host, sh.pane_id).then((why) => {
                  if (why) setNeeds({ paneId: sh.pane_id, why })
                  setTick((t) => t + 1)
                })
              }
            >
              ✕
            </button>
          </span>
        )
      })}
      <ConfirmDialog
        open={needs !== null}
        title={t('這顆 pane 還在做事')}
        body={<ServicePaneNote needs={needs?.why ?? null} />}
        confirmLabel={t('仍要關掉')}
        danger
        onCancel={() => setNeeds(null)}
        onConfirm={() => {
          const paneId = needs?.paneId
          setNeeds(null)
          if (paneId) void endHostShell(host, paneId, true).then(() => setTick((t) => t + 1))
        }}
      />
    </span>
  )
}

function HerdrVersionLine({ herdr }: { herdr: HerdrVersion }) {
  const v = herdrVersionView(herdr)
  return (
    <span className={`host-herdr ${v.level}`} title={v.hint || undefined} role={v.level === 'warn' ? 'alert' : undefined}>
      {v.text}
      {v.level === 'warn' && v.hint ? <span className="host-herdr-hint">{v.hint}</span> : null}
    </span>
  )
}

function BaselineFix({ fix, kind, command }: { fix: 'auto' | 'manual' | null; kind?: 'write' | 'install' | 'manual' | null; command?: string }) {
  const t = useT()
  if (fix === null) return null
  if (kind === 'write') return <span className="host-baseline-fix auto">{t('（將寫入）')}</span>
  if (kind === 'install') return <span className="host-baseline-fix auto" title={command}>{t('（將安裝）')}</span>
  if (kind === 'manual' && command) {
    return (
      <span className="host-baseline-fix manual">
        {t('（需手動）')}
        <code className="host-baseline-fix-cmd">{command}</code>
      </span>
    )
  }
  return <span className={`host-baseline-fix ${fix}`}>{fix === 'auto' ? t('（可自動補）') : t('（需手動）')}</span>
}

export function BaselineLine({ host = 'local', baseline, syncMode }: { host?: string; baseline: HostBaseline | null; syncMode?: 'off' | 'ask' | 'auto' }) {
  const t = useT()
  useUserTimeZone()
  const v = hostBaselineView(baseline)
  if (v.items.length === 0) {
    return (
      <span className={`host-baseline ${v.level}`}>
        <span className="host-baseline-header">
          <span className="host-baseline-head" title={v.hint || undefined} role={v.level === 'critical' ? 'alert' : undefined}>
            {v.text}
          </span>
          <HostBaselineRestore host={host} />
        </span>
      </span>
    )
  }

  const criticalItems = v.items.filter((i) => i.severity === 'critical')
  const warnItems = v.items.filter((i) => i.severity !== 'critical')
  const recheckTools = () => useStore.getState().refreshTools(host === 'local' ? '' : host)
  const osOf = baseline?.os ?? null

  return (
    <span className={`host-baseline ${v.level}`}>
      <span className="host-baseline-header">
        <span className="host-baseline-head" title={v.hint || undefined} role={v.level === 'critical' ? 'alert' : undefined}>
          {v.text}
        </span>
        <HostBaselineRestore host={host} />
      </span>
      <HostBaselineApply host={host} baseline={baseline} syncMode={syncMode} />
      {criticalItems.length > 0 ? (
        <ul className="host-baseline-items">
          {criticalItems.map((i) => (
            <li key={i.id} className={i.severity} title={i.id}>
              {i.message}
              <BaselineFix fix={i.fix} kind={i.kind} command={i.command} />
              {i.id.startsWith('tool.') ? <ToolInstallGuide tool={i.id.slice('tool.'.length)} os={osOf} onRecheck={recheckTools} /> : null}
            </li>
          ))}
        </ul>
      ) : null}
      {warnItems.length > 0 ? (
        <details className="env-fold">
          <summary>{criticalItems.length > 0 ? t('還有 {n} 項提醒', { n: warnItems.length }) : t('列出 {n} 項', { n: warnItems.length })}</summary>
          <ul className="host-baseline-items">
            {warnItems.map((i) => (
              <li key={i.id} className={i.severity} title={i.id}>
                {i.message}
                <BaselineFix fix={i.fix} kind={i.kind} command={i.command} />
                {i.note ? <span className="host-baseline-why">{t('選用：{note}', { note: i.note })}</span> : null}
                {baselineWarnWhy(i.id) ? <span className="host-baseline-why">{baselineWarnWhy(i.id)}</span> : null}
                {i.id.startsWith('tool.') ? <ToolInstallGuide tool={i.id.slice('tool.'.length)} os={osOf} onRecheck={recheckTools} /> : null}
              </li>
            ))}
          </ul>
        </details>
      ) : null}
    </span>
  )
}

function HostRow({ name }: { name: string }) {
  const t = useT()
  const host = useStore((s) => s.hosts.find((h) => h.name === name))
  const syncMode = useStore((s) => s.hostBaselineSync[name])
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
        aria-label={host.connected ? t('已連線') : t('未連線')}
        title={host.connected ? t('已連線') : (host.error ?? t('未連線'))}
      />
      <span className="host-main">
        <span className="host-name">
          {host.name}
          <span className="host-count">{projectCount > 0 ? t('{n} 個 Project', { n: projectCount }) : t('未使用')}</span>
        </span>
        <span className="host-ssh" title={`ssh ${host.ssh}${host.ssh_port !== 22 ? ` -p ${host.ssh_port}` : ''}`}>
          {host.ssh}
          {host.ssh_port !== 22 ? `:${host.ssh_port}` : ''} ・ {host.herdr_session}
        </span>
        {host.connected ? null : (
          <span className="host-err" title={host.error ?? ''}>
            {host.error ?? t('未連線')}
          </span>
        )}
        <HerdrVersionLine herdr={host.herdr} />
        <ToolBadges host={host.name} tools={host.tools} />
        <BaselineLine host={host.name} baseline={host.baseline} syncMode={syncMode} />
        <GhHostStatus host={host.name} />
        <HostShellList host={host.name} connected={host.connected} />
      </span>
      <span className="host-actions">
        <AttachButton command={host.attach_command} compact />
        <OpenShellButton host={host.name} connected={host.connected} />
        <button
          type="button"
          className="mini-btn"
          disabled={busy}
          aria-label={busy ? t('{name} 連線中', { name }) : t('重連 {name}', { name })}
          title={t('重新建立 ssh master 並 ping 遠端 herdr')}
          onClick={() => void reconnectHost(name)}
        >
          {busy ? t('連線中…') : t('重連')}
        </button>
        <button
          type="button"
          className="icon-btn"
          aria-label={t('刪除主機 {name}', { name })}
          title={projectCount > 0 ? t('仍有 Project 使用這個主機') : t('刪除主機')}
          onClick={() => setConfirmDelete(true)}
        >
          ✕
        </button>
      </span>
      <ConfirmDialog
        open={confirmDelete}
        title={t('刪除主機')}
        body={
          <>
            {t.x('要把 {name}（ssh {ssh}）從清單移除嗎？遠端的 herdr 與 agent 都不受影響，只是這台不再出現在 Project 的主機選項裡。', {
              name: <strong>{name}</strong>,
              ssh: `${host.ssh}${host.ssh_port !== 22 ? `:${host.ssh_port}` : ''}`,
            })}
            {projectCount > 0 ? (
              <>
                <br />
                <strong>{t('仍有 {n} 個 Project 在用這台主機，需先刪除或搬走那些 Project 才能移除。', { n: projectCount })}</strong>
              </>
            ) : null}
          </>
        }
        confirmLabel={t('刪除主機')}
        confirmDisabled={projectCount > 0}
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
  const t = useT()
  const addHost = useStore((s) => s.addHost)
  const [name, setName] = useState('')
  const [ssh, setSsh] = useState('')
  const [sshPort, setSshPort] = useState(String(HOST_DEFAULTS.ssh_port))
  const [session, setSession] = useState<string>(HOST_DEFAULTS.herdr_session)
  const [remotePath, setRemotePath] = useState<string>(HOST_DEFAULTS.remote_path)
  const [sshOpts, setSshOpts] = useState('')
  const [cleanAdvanced, setCleanAdvanced] = useState<{ sshPort: string; session: string; remotePath: string; sshOpts: string }>({
    sshPort: String(HOST_DEFAULTS.ssh_port),
    session: HOST_DEFAULTS.herdr_session,
    remotePath: HOST_DEFAULTS.remote_path,
    sshOpts: '',
  })
  const [advanced, setAdvanced] = useState(false)
  const [busy, setBusy] = useState(false)

  const nameOk = /^[a-z][a-z0-9_-]{0,31}$/.test(name) && name !== 'local'
  const sshHint = ssh.trim() ? sshTargetProblem(ssh.trim()) : null
  const portHint = portProblem(sshPort)
  const sessionHint = sessionProblem(session.trim() || HOST_DEFAULTS.herdr_session)
  const ok = nameOk && ssh.trim().length > 0 && !sshHint && !portHint && !sessionHint
  useUnsavedGuard(
    name !== '' ||
      ssh !== '' ||
      sshPort !== cleanAdvanced.sshPort ||
      session !== cleanAdvanced.session ||
      remotePath !== cleanAdvanced.remotePath ||
      sshOpts !== cleanAdvanced.sshOpts,
  )

  return (
    <form
      className="form"
      onSubmit={(e) => {
        e.preventDefault()
        if (!ok || busy) return
        const submitted = { name, sshText: ssh, sshPort, session, remotePath, sshOpts }
        setBusy(true)
        onResult(null)
        void addHost({
          name: submitted.name,
          ssh: submitted.sshText.trim(),
          ssh_port: Number(submitted.sshPort.trim()),
          herdr_session: submitted.session.trim() || HOST_DEFAULTS.herdr_session,
          remote_path: submitted.remotePath.trim(),
          ...(submitted.sshOpts.trim() ? { ssh_opts: submitted.sshOpts.trim().split(/\s+/) } : {}),
        }).then((res) => {
          setBusy(false)
          onResult(res)
          if (res) {
            setName((current) => (current === submitted.name ? '' : current))
            setSsh((current) => (current === submitted.sshText ? '' : current))
            setCleanAdvanced({ sshPort: submitted.sshPort, session: submitted.session, remotePath: submitted.remotePath, sshOpts: submitted.sshOpts })
          }
        })
      }}
    >
      <label className="field">
        <span>{t('名稱（唯一識別，用於 Project 的 host）')}</span>
        <input
          type="text"
          value={name}
          placeholder="my-server"
          spellCheck={false}
          onChange={(e) => setName(e.target.value.toLowerCase())}
        />
        {name && !nameOk ? (
          <span className="hint">{t('小寫英文字母開頭，之後可接小寫字母、數字、- 或 _，最多 32 個字；不能叫 local。')}</span>
        ) : null}
      </label>
      <label className="field">
        <span>{t('ssh 目標（可用 ssh_config 別名）')}</span>
        <input
          type="text"
          value={ssh}
          placeholder="user@my-server"
          spellCheck={false}
          onChange={(e) => setSsh(e.target.value)}
        />
        {sshHint ? <span className="hint">{sshHint}</span> : null}
      </label>

      <button type="button" className="disclosure sub" aria-expanded={advanced} onClick={() => setAdvanced(!advanced)}>
        <span className="chev">{advanced ? '▼' : '▶'}</span> {t('進階（都有預設值）')}
      </button>
      {}
      {!advanced && (portHint || sessionHint) ? <span className="hint">{t('進階欄位：{hints}', { hints: [portHint, sessionHint].filter(Boolean).join(t('；')) })}</span> : null}
      {advanced ? (
        <>
            <label className="field">
              <span>ssh_port</span>
              <input type="text" value={sshPort} spellCheck={false} onChange={(e) => setSshPort(e.target.value)} />
              {portHint ? <span className="hint">{portHint}</span> : null}
            </label>
            <label className="field">
              <span>{t('herdr_session（遠端 named session）')}</span>
              <input type="text" value={session} spellCheck={false} onChange={(e) => setSession(e.target.value)} />
              {sessionHint ? <span className="hint">{sessionHint}</span> : null}
            </label>
            <label className="field">
              <span>{t('remote_path（非互動 ssh shell 要前置的 PATH）')}</span>
              <input
                type="text"
                value={remotePath}
                spellCheck={false}
                onChange={(e) => setRemotePath(e.target.value)}
              />
            </label>
            <label className="field">
              <span>{t('ssh_opts（額外 ssh 參數，以空白分隔）')}</span>
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
        <span className="hint host-form-hint">{t('只用現有 ssh key（BatchMode），不會詢問密碼')}</span>
        <button type="submit" className="btn primary" disabled={!ok || busy}>
          {busy ? t('連線中…') : t('新增並連線')}
        </button>
      </div>
    </form>
  )
}

function LocalHostRow() {
  const t = useT()
  const connected = useStore((s) => s.connected)
  const tools = useStore((s) => s.localTools)
  const herdr = useStore((s) => s.localHerdr)
  const baseline = useStore((s) => s.localBaseline)
  const syncMode = useStore((s) => s.hostBaselineSync.local)
  const attach = useStore((s) => s.attachCommand)
  const projectCount = useStore((s) => s.projects.filter((p) => p.host === 'local').length)
  return (
    <div className="host-row local">
      <span className={`lamp lamp-${connected ? 'idle' : 'disconnected'}`} role="img" aria-label={connected ? t('herdr 已連線') : t('herdr 中斷')} title={connected ? t('herdr 已連線') : t('herdr 中斷')} />
      <span className="host-main">
        <span className="host-name">
          {t('本機')}
          <span className="host-count">{projectCount > 0 ? t('{n} 個 Project', { n: projectCount }) : t('未使用')}</span>
        </span>
        <span className="host-ssh">{attach}</span>
        <HerdrVersionLine herdr={herdr} />
        <ToolBadges host="local" tools={tools} />
        <BaselineLine host="local" baseline={baseline} syncMode={syncMode} />
        <GhHostStatus host="local" />
        <HostShellList host="local" connected={connected} />
      </span>
      <span className="host-actions">
        <AttachButton command={attach} compact />
        <OpenShellButton host="local" connected={connected} />
      </span>
    </div>
  )
}

function remoteCargoErr(e: unknown, t: ReturnType<typeof useT>): string {
  if (e instanceof ApiError && (e.status === 404 || e.status === 405)) {
    return t('這顆 daemon 還沒有 /api/build/remote（HTTP {status}），二進位比前端舊；要 cargo build --release 後重啟 daemon 才會有這個 API。', { status: e.status })
  }
  return e instanceof Error ? e.message : String(e)
}

function RemoteCargoPanel() {
  const t = useT()
  const [loaded, setLoaded] = useState(false)
  const [enabled, setEnabled] = useState(false)
  const [host, setHost] = useState('')
  const [user, setUser] = useState('')
  const [port, setPort] = useState('22')
  const [root, setRoot] = useState(DEFAULT_REMOTE_ROOT)
  const [saved, setSaved] = useState<RemoteCargoSettings | null>(null)
  const [needsToolchain, setNeedsToolchain] = useState(false)
  const [jobs, setJobs] = useState('4')
  const [password, setPassword] = useState('')
  const [passwordSet, setPasswordSet] = useState(false)
  const [clearPassword, setClearPassword] = useState(false)
  const [busy, setBusy] = useState(false)
  const [message, setMessage] = useState('')
  const [loadFailed, setLoadFailed] = useState(false)
  const [reload, setReload] = useState(0)
  const lock = useRef(nestableBusy(setBusy)).current

  useEffect(() => {
    let alive = true
    void api.fetchRemoteCargoSettings().then(
      (cfg) => {
        if (!alive) return
        setLoadFailed(false)
        setMessage('')
        setEnabled(cfg.enabled)
        setHost(cfg.host)
        setUser(cfg.user)
        setPort(String(cfg.ssh_port))
        setRoot(cfg.remote_root)
        setJobs(String(cfg.cargo_jobs))
        setPasswordSet(cfg.password_set)
        setSaved(cfg)
        setLoaded(true)
      },
      (e) => {
        if (alive) {
          setLoadFailed(true)
          setMessage(t('讀取外部 Cargo 設定失敗：{error}', { error: remoteCargoErr(e, t) }))
          setLoaded(true)
        }
      },
    )
    return () => {
      alive = false
    }
  }, [reload, t])

  const form = { enabled, host, user, port, root, jobs, password, clearPassword }
  const problems = remoteCargoProblems(form)
  const blocked = problems.length > 0
  const unsaved = saved
    ? remoteCargoUnsaved(saved, form)
    : enabled || host !== '' || user !== '' || port !== '22' || root !== DEFAULT_REMOTE_ROOT || jobs !== '4' || password !== '' || clearPassword
  useUnsavedGuard(unsaved)

  const persist = async (): Promise<boolean> => {
    setMessage('')
    const submitted = form
    try {
      const cfg = await api.saveRemoteCargoSettings(toRemoteCargoInput(submitted))
      setPassword((current) => (current === submitted.password ? '' : current))
      setClearPassword((current) => (current === submitted.clearPassword ? false : current))
      setPasswordSet(cfg.password_set)
      setSaved(cfg)
      setMessage(t('✓ 已儲存。新啟動的本機 Bot 會自動使用外部 Cargo verification。'))
      return true
    } catch (e) {
      setMessage(t('儲存失敗：{error}', { error: remoteCargoErr(e, t) }))
      return false
    }
  }

  const save = async (): Promise<boolean> => {
    lock.begin()
    try {
      return await persist()
    } finally {
      lock.end()
    }
  }

  const test = async () => {
    lock.begin()
    setMessage('')
    try {
      const unsaved = !saved || remoteCargoUnsaved(saved, form)
      if (unsaved && !(await persist())) return
      const r = await api.testRemoteCargo()
      const how = r.password_auth ? t('密碼') : 'SSH key/agent'
      const target = `${user.trim()}@${host.trim()}${(Number(port) || 22) === 22 ? '' : `:${Number(port)}`}`
      const first = unsaved ? t('（已先儲存表單裡的設定）') : ''
      setNeedsToolchain(r.cargo_missing || r.clippy_missing)
      setMessage(
        r.cargo_missing
          ? t('✓ {target} 連得上（{how}）{first}，但這台還沒有 Rust 工具鏈（{platform}）。按下面「安裝 Rust 工具鏈」由 daemon 用 rustup 裝（minimal，不會改遠端的 shell profile）。', {
              target,
              how,
              first,
              platform: `${r.os || '?'}/${r.arch || '?'}`,
            })
          : r.clippy_missing
            ? t('✓ {target} 連得上（{how}）{first}，有 cargo 但沒有 clippy——cargo clippy 轉過去會失敗。按下面「安裝 Rust 工具鏈」補上。\n{output}', {
                target,
                how,
                first,
                output: r.output,
              })
            : t('✓ {target} SSH/Cargo 可用（${how}）{first}：\n{output}', {
                target,
                how,
                first,
                output: r.output,
              })
      )
    } catch (e) {
      setNeedsToolchain(false)
      setMessage(t('測試失敗：{error}', { error: remoteCargoErr(e, t) }))
    } finally {
      lock.end()
    }
  }

  const installToolchain = async () => {
    setBusy(true)
    setMessage(t('安裝中…（rustup minimal，第一次大約要一兩分鐘）'))
    try {
      const r = await api.installRemoteCargoToolchain()
      setNeedsToolchain(false)
      const head = r.already_installed
        ? t('✓ 這台本來就有了：{version}', { version: r.cargo_version })
        : t('✓ 已安裝：{version}', { version: r.cargo_version })
      const warnings = [
        r.cc_missing ? t('⚠ 這台沒有 C 編譯器（cc／gcc），cargo test 連結會失敗。請在遠端裝 build-essential（Debian/Ubuntu）或 gcc。') : '',
        r.clippy_missing ? t('⚠ 這台的 cargo 沒有 clippy、也補不上（不是 rustup 裝的？），cargo clippy 轉過去會失敗。請在遠端裝 clippy。') : '',
      ].filter(Boolean)
      setMessage([head, ...warnings].join('\n'))
    } catch (e) {
      setMessage(t('安裝失敗：{error}', { error: remoteCargoErr(e, t) }))
    } finally {
      setBusy(false)
    }
  }

  if (!loaded) return <p className="hint">{t('讀取外部 Cargo 設定…')}</p>

  return (
    <section className="remote-cargo-settings">
      <div className="settings-subhead">
        <strong>{t('外部 Cargo 主機')}</strong>
        <span className="hint">{t('開發者功能：把 check/test/clippy 移到另一台 SSH 主機，build 仍留本機。')}</span>
      </div>
      <label className="field checkbox-field">
        <input type="checkbox" checked={enabled} onChange={(e) => setEnabled(e.target.checked)} />
        <span>{t('啟用外部 Cargo verification')}</span>
      </label>
      {}
      {enabled ? (
        <>
          <div className="remote-cargo-grid">
            <label className="field">
              <span>{t('主機 / IP')}</span>
              <input value={host} placeholder="192.168.1.20" spellCheck={false} onChange={(e) => setHost(e.target.value)} />
            </label>
            <label className="field">
              <span>{t('SSH 帳號')}</span>
              <input value={user} placeholder="builder" spellCheck={false} onChange={(e) => setUser(e.target.value)} />
            </label>
            <label className="field">
              <span>{t('SSH port')}</span>
              <input value={port} spellCheck={false} onChange={(e) => setPort(e.target.value)} />
            </label>
            <label className="field">
              <span>{t('遠端 cargo jobs')}</span>
              <input value={jobs} spellCheck={false} onChange={(e) => setJobs(e.target.value)} />
            </label>
          </div>
          <label className="field">
            <span>{t('遠端工作目錄')}</span>
            <input value={root} spellCheck={false} onChange={(e) => setRoot(e.target.value)} />
          </label>
          <label className="field">
            <span>{t('SSH 密碼（可留空使用 key / ssh-agent）')}</span>
            <input
              type="password"
              value={password}
              autoComplete="new-password"
              placeholder={passwordSet ? t('已安全儲存；留空不變') : t('未設定')}
              onChange={(e) => {
                setPassword(e.target.value)
                if (e.target.value) setClearPassword(false)
              }}
            />
            <span className="hint">
              {t('密碼不寫入 config.toml、不回傳前端；daemon 只存 0600 secret file。密碼模式優先用 sshpass，沒有就走 ssh 自己的 askpass（OpenSSH 8.4+）。')}
            </span>
          </label>
          {passwordSet ? (
            <label className="field checkbox-field">
              <input type="checkbox" checked={clearPassword} onChange={(e) => setClearPassword(e.target.checked)} />
              <span>{t('清除已存密碼，改用 SSH key / agent')}</span>
            </label>
          ) : null}
        </>
      ) : null}
      <div className="form-actions">
        {enabled ? (
          <button type="button" className="btn" disabled={busy || blocked || loadFailed || !host.trim() || !user.trim()} onClick={() => void test()}>
            {t('測試連線')}
          </button>
        ) : null}
        {}
        <button
          type="button"
          className="btn primary"
          disabled={busy || blocked || (enabled && (!host.trim() || !user.trim())) || (!saved && !unsaved)}
          title={!saved && !unsaved ? t('讀不到目前的設定，先重新讀取') : undefined}
          onClick={() => void save()}
        >
          {busy ? t('處理中…') : t('儲存')}
        </button>
        {needsToolchain ? (
          <button type="button" className="btn" disabled={busy} onClick={() => void installToolchain()}>
            {t('安裝 Rust 工具鏈')}
          </button>
        ) : null}
      </div>
      {problems.length > 0 ? (
        <ul className="hint field-problems" role="alert">
          {problems.map((p) => (
            <li key={p.field}>{p.text}</li>
          ))}
        </ul>
      ) : null}
      {}
      <div role="status" aria-live="polite">
        {message ? (
          <pre className="host-result" role={/^(讀取外部 Cargo 設定|儲存|測試|安裝)失敗：/.test(message) ? 'alert' : undefined}>
            {message}
          </pre>
        ) : null}
      </div>
      {loadFailed ? (
        <>
          <p className="hint">{t('讀不到 daemon 目前存的設定。這時按儲存會用這份表單整份取代（沒填的欄位用預設值）；先按「重新讀取」。')}</p>
          <button type="button" className="btn" disabled={busy} onClick={() => setReload((n) => n + 1)}>
            {t('重新讀取')}
          </button>
        </>
      ) : null}
    </section>
  )
}

export function HostsPanel() {
  const t = useT()
  const hosts = useStore((s) => s.hosts)
  const [result, setResult] = useState<HostResult | null>(null)

  return (
    <div className="hosts-panel">
      <div className="host-list">
        <LocalHostRow />
      </div>
      {hosts.length === 0 ? (
        <p className="hint hosts-empty">{t('尚未設定遠端主機。Project 預設都在本機。')}</p>
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
            ? t('✓ {name}：ssh master 與遠端 herdr 都就緒', { name: result.name })
            : t('✕ {name}：{error}', { name: result.name, error: result.error ?? t('連線失敗') })}
        </div>
      ) : null}
      <NewHostForm onResult={setResult} />
      <RemoteCargoPanel />
    </div>
  )
}
