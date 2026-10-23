import { useRef, useState } from 'react'
import type { BotKind, IdentityStatus, IdentityStatusMap } from '../api/types'
import { useT } from '../lib/i18n'
import { identityDisabled, useStore } from '../store/store'
import { findIdentity, hostLabel, identityHost, identityRowKey, identityUseCount, shadowedByConfig } from '../store/identityRows'
import { ConfirmDialog } from './ConfirmDialog'
import { KindTag } from './KindTag'
import './identitiesPanel.css'

import { checkEnvText, emptyEnvKeys, envDisplayText } from './identityEnv'
import { useUnsavedGuard } from '../hooks/useUnsavedGuard'
/** 身份預設（例如 `cc1` = 另一個 `CLAUDE_CONFIG_DIR`）：daemon 啟動 bot 時注入 `env`；`args` 契約仍在但 UI 不提供輸入。 */

export function IdentityBadge({
  name,
  showDefault,
  kind,
  unknown,
}: {
  name: string | null
  showDefault?: boolean
  kind?: BotKind
  /** active run 有但 daemon 沒記 runtime_identity；不要拿設定值冒充實際值。 */
  unknown?: boolean
}) {
  const t = useT()
  if (unknown) {
    return (
      <span className="identity-badge is-unknown" title={t('身份：未知（這個 Run 沒有記錄啟動身份）')}>
        {t('未知')}
      </span>
    )
  }
  // 沒指定身份也要標出來，不然本機預設和 cc1 在列表上長得一樣。
  if (!name) {
    if (!showDefault) return null
    // claude 的本機預設帳號就是 cc0；其他 kind 沒有這套身份。
    if (kind && kind !== 'claude') {
      return (
        <span className="identity-badge is-default" title={t('身份：不指定（本機預設）')}>
          {t('預設')}
        </span>
      )
    }
    return (
      <span className="identity-badge" title={t('身份：不指定（本機預設 cc0）')}>
        cc0
      </span>
    )
  }
  return (
    <span className="identity-badge" title={t('身份：{name}', { name })}>
      {name}
    </span>
  )
}

/** 身份在每台主機各自的登入狀態（帳號是每台各自登入的）；`null` = 問不到，標「未知」而非「未登入」。 */
function IdentityHostLogins({ name, kind, only }: { name: string; kind?: string; only?: string }) {
  const t = useT()
  const localStatus = useStore((s) => s.localIdentityStatus[name])
  const localConnected = useStore((s) => s.connected)
  const allHosts = useStore((s) => s.hosts)
  // 明寫 host 的身份只屬於那一台：別台同名的登入狀態是別的帳號，不能掛在這一列。
  const pinned = only && only !== 'local' ? only : null
  const hosts = pinned ? allHosts.filter((h) => h.name === pinned) : allHosts
  const rows: { host: string; label: string; state: boolean | null; account: string | null; reason: string | null; connected: boolean }[] = pinned
    ? []
    : [{ host: 'local', label: t('本機'), state: localStatus?.logged_in ?? null, account: localStatus?.account ?? null, reason: localStatus?.reason ?? null, connected: localConnected }]
  for (const h of hosts) {
    const st = h.identity_status[name]
    rows.push({ host: h.name, label: h.name, state: h.connected ? (st?.logged_in ?? null) : null, account: st?.account ?? null, reason: h.connected ? st?.reason ?? null : t('主機未連線'), connected: h.connected })
  }
  return (
    <span className="identity-logins">
      {rows.map((r) => {
        const text = r.state === true ? t('已登入') : r.state === false ? t('未登入') : t('未知')
        const title =
          r.state === true
            ? r.account
              ? t('{label}：已登入（{account}）', { label: r.label, account: r.account })
              : t('{label}：已登入', { label: r.label })
            : r.state === false
              ? t('{label}：這個身份沒有登入，用它啟動的 bot 會停在登入畫面', { label: r.label })
              : t('{label}：問不到登入狀態（{reason}）', { label: r.label, reason: r.reason ?? t('尚未偵測過') })
        const chip = (
          <span
            className={`identity-login is-${r.state === true ? 'ok' : r.state === false ? 'out' : 'unknown'}`}
            title={title}
          >
            {r.label} {text}
          </span>
        )
        return (
          <span key={r.host} className="identity-login-wrap">
            {chip}
            {r.connected ? (
              <>
                <IdentityLoginButton host={r.host} identity={name} loggedIn={r.state === true} />
                {r.state === true ? <IdentityLogoutButton host={r.host} identity={name} /> : null}
                {kind ? <IdentityDisableButton host={r.host} kind={kind} name={name} /> : null}
              </>
            ) : null}
          </span>
        )
      })}
    </span>
  )
}

function closeEnclosingPopup(from: HTMLElement) {
  if (!from.closest('.modal-backdrop')) return
  from.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
}

function IdentityLoginButton({ host, identity, loggedIn }: { host: string; identity: string; loggedIn: boolean }) {
  const t = useT()
  const loginIdentity = useStore((s) => s.loginIdentity)
  const notify = useStore((s) => s.notify)
  const [busy, setBusy] = useState(false)
  return (
    <button
      type="button"
      className="mini-btn identity-login-btn"
      disabled={busy}
      title={t('在{host}開臨時 pane，帶著 {identity} 的設定執行登入', { host: hostLabel(host), identity })}
      onClick={(e) => {
        if (busy) return
        const btn = e.currentTarget
        setBusy(true)
        void loginIdentity(host, identity)
          .then((ok) => {
            if (ok) closeEnclosingPopup(btn)
          })
          .catch((err) => notify('error', err instanceof Error ? err.message : String(err)))
          .finally(() => setBusy(false))
      }}
    >
      {busy ? t('登入中…') : loggedIn ? t('切換') : t('登入')}
    </button>
  )
}

/** 登出走跟登入同一條路（臨時 pane），但會清掉那個帳號的憑證，所以先問一次。 */
function IdentityLogoutButton({ host, identity }: { host: string; identity: string }) {
  const t = useT()
  const logoutIdentity = useStore((s) => s.logoutIdentity)
  const notify = useStore((s) => s.notify)
  const used = useStore((s) => identityUseCount(s.bots, s.projects, host, identity))
  const [busy, setBusy] = useState(false)
  const [confirm, setConfirm] = useState(false)
  // 確認框的 onConfirm 沒有按鈕事件可用：先把按鈕元素存起來，登出成功後收掉包著這個面板的「環境設定」彈窗（跟登入同一條）。
  const btnRef = useRef<HTMLButtonElement>(null)
  return (
    <>
      <button
        ref={btnRef}
        type="button"
        className="mini-btn identity-login-btn"
        disabled={busy}
        title={t('在{host}開臨時 pane，帶著 {identity} 的設定登出這個帳號', { host: hostLabel(host), identity })}
        onClick={() => setConfirm(true)}
      >
        {busy ? t('登出中…') : t('登出')}
      </button>
      <ConfirmDialog
        open={confirm}
        title={t('登出 {identity}', { identity })}
        body={
          <>
            {t.x('會在{host}開一個臨時 pane，帶著 {identity} 的設定目錄執行登出，清掉這個帳號的憑證。', {
              host: hostLabel(host),
              identity: <strong>{identity}</strong>,
            })}
            {used > 0 ? (
              <>
                <br />
                <strong>{t('目前有 {n} 顆 Bot 用這個身份', { n: used })}</strong>
                {t('：正在跑的不受影響，但之後重新啟動會停在登入畫面。')}
              </>
            ) : null}
          </>
        }
        confirmLabel={t('登出')}
        danger
        onCancel={() => setConfirm(false)}
        onConfirm={() => {
          setConfirm(false)
          setBusy(true)
          const btn = btnRef.current
          void logoutIdentity(host, identity)
            .then((ok) => {
              if (ok && btn) closeEnclosingPopup(btn)
            })
            .catch((err) => notify('error', err instanceof Error ? err.message : String(err)))
            .finally(() => setBusy(false))
        }}
      />
    </>
  )
}

/** 停用＝之後挑身份時看不到它（daemon 記在 `identity_prefs`），已經綁著它的 Bot 不動。 */
function IdentityDisableButton({ host, kind, name }: { host: string; kind: string; name: string }) {
  const t = useT()
  const disabled = useStore((s) => identityDisabled(s, host, kind, name))
  const setIdentityDisabled = useStore((s) => s.setIdentityDisabled)
  const busy = useStore((s) => s.busy[`identity-disabled:${host || 'local'}|${kind}|${name}`] === true)
  return (
    <button
      type="button"
      className={`mini-btn identity-login-btn${disabled ? ' is-off' : ''}`}
      disabled={busy}
      title={
        disabled
          ? t('重新啟用：{name} 會再出現在{host}的身份選單裡', { name, host: hostLabel(host) })
          : t('標為停用：{name} 不再出現在{host}的身份選單與自動挑選裡，已經在用它的 Bot 不受影響', { name, host: hostLabel(host) })
      }
      onClick={() => void setIdentityDisabled(host, kind, name, !disabled)}
    >
      {disabled ? t('啟用') : t('停用')}
    </button>
  )
}

/** 停用中的身份在列表上要一眼看得出來。 */
function DisabledChip({ host, kind, name }: { host: string; kind: string; name: string }) {
  const t = useT()
  const disabled = useStore((s) => identityDisabled(s, host, kind, name))
  if (!disabled) return null
  return (
    <span className="identity-disabled-chip" title={t('{name} 已停用：挑身份時不會出現，已經在用它的 Bot 不受影響', { name })}>
      {t('停用')}
    </span>
  )
}

function IdentityRow({ host, name }: { host: string; name: string }) {
  const t = useT()
  const ident = useStore((s) => findIdentity(s.identities, host, name))
  const used = useStore((s) => identityUseCount(s.bots, s.projects, host, name))
  const removeIdentity = useStore((s) => s.removeIdentity)
  const [confirmDelete, setConfirmDelete] = useState(false)
  if (!ident) return null
  const envText = envDisplayText(ident.env)
  const emptyKeys = emptyEnvKeys(ident.env)
  const keyList = emptyKeys.join(t('、'))
  return (
    <div className="identity-row">
      <span className="identity-main">
        <span className="identity-name">
          <IdentityBadge name={ident.name} />
          <KindTag kind={ident.kind} />
          {/* 身份是每台一份（SPEC §16.2）：同名的 cc1 在別台是別的帳號，列上要看得出是哪一台。 */}
          <span className="host-count">{hostLabel(host)}</span>
          <span className="host-count">{used > 0 ? t('{n} 個 Bot', { n: used }) : t('未使用')}</span>
          <DisabledChip host={host} kind={ident.kind} name={ident.name} />
        </span>
        {emptyKeys.length > 0 ? (
          <span className="identity-empty-env" role="alert" title={t('空值等於沒設；要改請到 config.toml 該身份的 [identities.env] 寫上值，或刪掉這個身份重建')}>
            {t('⚠ env {keys} 的值是空的，這個身份會悄悄用預設帳號', { keys: keyList })}
          </span>
        ) : null}
        <span className="identity-detail" title={envText}>
          {envText ? envText.replace(/\n/g, ' ・ ') : t('（無 env）')} {/* i18n-ignore：env 分隔符 */}
        </span>
        <IdentityHostLogins name={ident.name} kind={ident.kind} only={host} />
      </span>
      <button
        type="button"
        className="icon-btn"
        aria-label={t('刪除身份 {name}', { name })}
        title={used > 0 ? t('仍有 Bot 使用這個身份') : t('刪除身份')}
        onClick={() => setConfirmDelete(true)}
      >
        ✕
      </button>
      <ConfirmDialog
        open={confirmDelete}
        title={t('刪除身份')}
        body={
          <>
            {t.x('要把{host}的身份 {name} 移除嗎？別台同名的不受影響，已經登入的帳號本身也不受影響。', {
              host: <strong>{hostLabel(host)}</strong>,
              name: <strong>{name}</strong>,
            })}
            {used > 0 ? (
              <>
                <br />
                <strong>{t('{host}仍有 {n} 個 Bot 綁著這個身份', { host: hostLabel(host), n: used })}</strong>
                {t('，需先把那些 Bot 改用別的身份或移除。')}
              </>
            ) : (
              t(' 目前沒有 Bot 在用它。')
            )}
          </>
        }
        confirmLabel={t('刪除身份')}
        confirmDisabled={used > 0}
        danger
        onCancel={() => setConfirmDelete(false)}
        onConfirm={() => {
          setConfirmDelete(false)
          void removeIdentity(name, host)
        }}
      />
    </div>
  )
}

/** 各 kind 的設定目錄變數（SPEC §16／§12）：claude 用 CLAUDE_CONFIG_DIR、codex 用 CODEX_HOME、grok 用 GROK_HOME。 */
const ENV_PREFILL: Record<BotKind, string> = {
  claude: 'CLAUDE_CONFIG_DIR=$HOME/.claude-',
  codex: 'CODEX_HOME=$HOME/.codex-',
  grok: 'GROK_HOME=$HOME/.grok-',
  // agy 的設定目錄只認 `$HOME`、沒有環境變數可換（第一階段單一身份，這個表單不提供 agy）。
  agy: '',
}

function NewIdentityForm() {
  const t = useT()
  const addIdentity = useStore((s) => s.addIdentity)
  const [name, setName] = useState('')
  const [kind, setKind] = useState<BotKind>('claude')
  const [envText, setEnvText] = useState(ENV_PREFILL.claude)
  const [cleanPreset, setCleanPreset] = useState({ kind: 'claude' as BotKind, envText: ENV_PREFILL.claude })
  const [busy, setBusy] = useState(false)
  const nameOk = /^[a-z][a-z0-9_-]{0,31}$/.test(name)
  const envCheck = checkEnvText(envText)
  useUnsavedGuard(name !== '' || kind !== cleanPreset.kind || envText !== cleanPreset.envText)

  return (
    <form
      className="form"
      onSubmit={(e) => {
        e.preventDefault()
        if (!nameOk || busy || envCheck.errors.length > 0) return
        const submitted = { name, kind, envText }
        setBusy(true)
        void addIdentity({ name: submitted.name, kind: submitted.kind, env: envCheck.env }).then((ok) => {
          setBusy(false)
          if (ok) {
            setName((current) => (current === submitted.name ? '' : current))
            setCleanPreset({ kind: submitted.kind, envText: submitted.envText })
          }
        })
      }}
    >
      <label className="field">
        <span>{t('名稱（例如 cc1）')}</span>
        <input
          type="text"
          value={name}
          placeholder="cc1"
          spellCheck={false}
          onChange={(e) => setName(e.target.value.toLowerCase())}
        />
        {name && !nameOk ? (
          <span className="hint">{t('小寫英文字母開頭，之後可接小寫字母、數字、- 或 _，最多 32 個字。')}</span>
        ) : null}
      </label>
      <label className="field">
        <span>kind</span>
        <select
          value={kind}
          onChange={(e) => {
            const next = e.target.value as BotKind
            // 使用者沒改過那一行才跟著換 kind 的設定目錄變數，免得蓋掉打好的。
            setEnvText((cur) => (cur === ENV_PREFILL[kind] ? ENV_PREFILL[next] : cur))
            setKind(next)
          }}
        >
          <option value="claude">claude</option>
          <option value="codex">codex</option>
          <option value="grok">grok</option>
        </select>
      </label>
      <label className="field">
        <span>{t.x('env（每行 KEY=VALUE；{home} 與開頭 {tilde} 會以該主機的家目錄展開；claude 用 CLAUDE_CONFIG_DIR、grok 用 GROK_HOME）', { home: '$HOME', tilde: '~' })}</span>
        <textarea rows={3} value={envText} spellCheck={false} onChange={(e) => setEnvText(e.target.value)} />
        {envCheck.errors.length > 0 ? (
          <ul className="hint env-errors" role="alert">
            {envCheck.errors.map((e) => (
              <li key={e}>{e}</li>
            ))}
          </ul>
        ) : null}
      </label>
      <div className="form-actions">
        <button type="submit" className="btn primary" disabled={!nameOk || busy || envCheck.errors.length > 0}>
          {t('新增身份')}
        </button>
      </div>
    </form>
  )
}

/** 從各主機 shell 認出來的 `ccN`（SPEC §16），唯讀；同名 config 身份會蓋過它，已列出的不重複。 */
function ShellIdentities() {
  const t = useT()
  const configured = useStore((s) => s.identities)
  const local = useStore((s) => s.localIdentityStatus)
  const hosts = useStore((s) => s.hosts)
  const rows: { host: string; label: string; st: IdentityStatus }[] = []
  const collect = (host: string, label: string, map: IdentityStatusMap) => {
    for (const st of Object.values(map)) {
      if (st.source !== 'shell') continue
      // 只有同一台的 config 才蓋得掉：本機的 `cc1` 不能把 m4p 認出來的 `cc1`（別的帳號）藏起來。
      if (shadowedByConfig(configured, host, st.name)) continue
      rows.push({ host, label, st })
    }
  }
  collect('local', t('本機'), local)
  for (const h of hosts) collect(h.name, h.name, h.identity_status)
  if (rows.length === 0) return null
  return (
    <div className="identity-shell-block">
      <p className="hint">
        {t.x('以下是從各主機登入 shell 的 {alias} alias 認出來的身份（{rc} 等），可以直接指派給 Bot；要改就改那台主機的 alias。「停用」只是把它從身份選單與自動挑選裡拿掉（alias 還在），已經在用它的 Bot 不受影響。', {
          alias: <code>ccN</code>,
          rc: <code>~/.zshrc</code>,
        })}
      </p>
      {rows.map(({ host, label, st }) => (
        <div className="identity-row is-shell" key={`${host}:${st.name}`}>
          <span className="identity-main">
            <span className="identity-name">
              <IdentityBadge name={st.name} />
              <KindTag kind={st.kind} />
              <span className="host-count">{label}</span>
              <DisabledChip host={host} kind={st.kind} name={st.name} />
            </span>
            <span className="identity-detail" title={st.config_dir ?? t('預設帳號（無 CLAUDE_CONFIG_DIR）')}>
              {st.config_dir ? `CLAUDE_CONFIG_DIR=${st.config_dir}` : t('（預設帳號）')}
            </span>
            {/* 同一種 chip 容器，否則單獨一顆會被 flex 拉成整行寬。 */}
            <span className="identity-logins">
              <span className="identity-login-wrap">
                <span
                  className={`identity-login is-${st.logged_in === true ? 'ok' : st.logged_in === false ? 'out' : 'unknown'}`}
                  title={st.logged_in === null ? st.reason ?? t('登入狀態未知') : st.account ?? undefined}
                >
                  {label} {st.logged_in === true ? t('已登入') : st.logged_in === false ? t('未登入') : t('未知')}
                </span>
                <IdentityLoginButton host={host} identity={st.name} loggedIn={st.logged_in === true} />
                {st.logged_in === true ? <IdentityLogoutButton host={host} identity={st.name} /> : null}
                <IdentityDisableButton host={host} kind={st.kind} name={st.name} />
              </span>
            </span>
          </span>
        </div>
      ))}
    </div>
  )
}

export function IdentitiesPanel() {
  const t = useT()
  const identities = useStore((s) => s.identities)
  return (
    <div className="hosts-panel identities-panel">
      {identities.length === 0 ? <p className="hint">{t('尚未定義任何身份。')}</p> : null}
      {identities.map((i) => (
        <IdentityRow key={identityRowKey(i)} host={identityHost(i)} name={i.name} />
      ))}
      <ShellIdentities />
      <NewIdentityForm />
    </div>
  )
}
