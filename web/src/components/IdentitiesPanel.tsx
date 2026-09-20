import { useState } from 'react'
import type { BotKind, IdentityStatus, IdentityStatusMap } from '../api/types'
import { identityDisabled, useStore } from '../store/store'
import { findIdentity, hostLabel, identityHost, identityRowKey, identityUseCount, shadowedByConfig } from '../store/identityRows'
import { ConfirmDialog } from './ConfirmDialog'
import { KindTag } from './KindTag'
import './identitiesPanel.css'

import { parseEnvText, envToText } from './identityEnv'
/** 身份預設（例如 `cc1` = 另一個 `CLAUDE_CONFIG_DIR`）：daemon 啟動 bot 時注入 `env`；`args` 契約仍在但 UI 不提供輸入。 */

export function IdentityBadge({
  name,
  showDefault,
  kind,
}: {
  name: string | null
  showDefault?: boolean
  kind?: BotKind
}) {
  // 沒指定身份也要標出來，不然本機預設和 cc1 在列表上長得一樣。
  if (!name) {
    if (!showDefault) return null
    // claude 的本機預設帳號就是 cc0；其他 kind 沒有這套身分。
    if (kind && kind !== 'claude') {
      return (
        <span className="identity-badge is-default" title="身份：不指定（本機預設）">
          預設
        </span>
      )
    }
    return (
      <span className="identity-badge" title="身份：不指定（本機預設 cc0）">
        cc0
      </span>
    )
  }
  return (
    <span className="identity-badge" title={`身份：${name}`}>
      {name}
    </span>
  )
}

/** 身份在每台主機各自的登入狀態（帳號是每台各自登入的）；`null` = 問不到，標「未知」而非「未登入」。 */
function IdentityHostLogins({ name, kind, only }: { name: string; kind?: string; only?: string }) {
  const localStatus = useStore((s) => s.localIdentityStatus[name])
  const localConnected = useStore((s) => s.connected)
  const allHosts = useStore((s) => s.hosts)
  // 明寫 host 的身分只屬於那一台：別台同名的登入狀態是別的帳號，不能掛在這一列。
  const pinned = only && only !== 'local' ? only : null
  const hosts = pinned ? allHosts.filter((h) => h.name === pinned) : allHosts
  const rows: { host: string; label: string; state: boolean | null; account: string | null; reason: string | null; connected: boolean }[] = pinned
    ? []
    : [{ host: 'local', label: '本機', state: localStatus?.logged_in ?? null, account: localStatus?.account ?? null, reason: localStatus?.reason ?? null, connected: localConnected }]
  for (const h of hosts) {
    const st = h.identity_status[name]
    rows.push({ host: h.name, label: h.name, state: h.connected ? (st?.logged_in ?? null) : null, account: st?.account ?? null, reason: h.connected ? st?.reason ?? null : '主機未連線', connected: h.connected })
  }
  return (
    <span className="identity-logins">
      {rows.map((r) => {
        const text = r.state === true ? '已登入' : r.state === false ? '未登入' : '未知'
        const title =
          r.state === true
            ? `${r.label}：已登入${r.account ? `（${r.account}）` : ''}`
            : r.state === false
              ? `${r.label}：這個身份沒有登入，用它啟動的 bot 會停在登入畫面`
              : `${r.label}：問不到登入狀態（${r.reason ?? '尚未偵測過'}）`
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
  const loginIdentity = useStore((s) => s.loginIdentity)
  const notify = useStore((s) => s.notify)
  const [busy, setBusy] = useState(false)
  return (
    <button
      type="button"
      className="mini-btn identity-login-btn"
      disabled={busy}
      title={`在${hostLabel(host)}開臨時 pane，帶著 ${identity} 的設定執行登入`}
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
      {busy ? '登入中…' : loggedIn ? '切換' : '登入'}
    </button>
  )
}

/** 登出走跟登入同一條路（臨時 pane），但會清掉那個帳號的憑證，所以先問一次。 */
function IdentityLogoutButton({ host, identity }: { host: string; identity: string }) {
  const logoutIdentity = useStore((s) => s.logoutIdentity)
  const notify = useStore((s) => s.notify)
  const used = useStore((s) => identityUseCount(s.bots, s.projects, host, identity))
  const [busy, setBusy] = useState(false)
  const [confirm, setConfirm] = useState(false)
  return (
    <>
      <button
        type="button"
        className="mini-btn identity-login-btn"
        disabled={busy}
        title={`在${hostLabel(host)}開臨時 pane，帶著 ${identity} 的設定登出這個帳號`}
        onClick={() => setConfirm(true)}
      >
        {busy ? '登出中…' : '登出'}
      </button>
      <ConfirmDialog
        open={confirm}
        title={`登出 ${identity}`}
        body={
          <>
            會在{hostLabel(host)}開一個臨時 pane，帶著 <strong>{identity}</strong> 的設定目錄執行登出，清掉這個帳號的憑證。
            {used > 0 ? (
              <>
                <br />
                <strong>目前有 {used} 顆 Bot 用這個身份</strong>：正在跑的不受影響，但之後重新啟動會停在登入畫面。
              </>
            ) : null}
          </>
        }
        confirmLabel="登出"
        danger
        onCancel={() => setConfirm(false)}
        onConfirm={() => {
          setConfirm(false)
          setBusy(true)
          void logoutIdentity(host, identity)
            .catch((err) => notify('error', err instanceof Error ? err.message : String(err)))
            .finally(() => setBusy(false))
        }}
      />
    </>
  )
}

/** 停用＝之後挑身份時看不到它（daemon 記在 `identity_prefs`），已經綁著它的 Bot 不動。 */
function IdentityDisableButton({ host, kind, name }: { host: string; kind: string; name: string }) {
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
          ? `重新啟用：${name} 會再出現在${hostLabel(host)}的身份選單裡`
          : `標為停用：${name} 不再出現在${hostLabel(host)}的身份選單與自動挑選裡，已經在用它的 Bot 不受影響`
      }
      onClick={() => void setIdentityDisabled(host, kind, name, !disabled)}
    >
      {disabled ? '啟用' : '停用'}
    </button>
  )
}

/** 停用中的身份在列表上要一眼看得出來。 */
function DisabledChip({ host, kind, name }: { host: string; kind: string; name: string }) {
  const disabled = useStore((s) => identityDisabled(s, host, kind, name))
  if (!disabled) return null
  return (
    <span className="identity-disabled-chip" title={`${name} 已停用：挑身份時不會出現，已經在用它的 Bot 不受影響`}>
      停用
    </span>
  )
}

function IdentityRow({ host, name }: { host: string; name: string }) {
  const ident = useStore((s) => findIdentity(s.identities, host, name))
  const used = useStore((s) => identityUseCount(s.bots, s.projects, host, name))
  const removeIdentity = useStore((s) => s.removeIdentity)
  const [confirmDelete, setConfirmDelete] = useState(false)
  if (!ident) return null
  const envText = envToText(ident.env)
  return (
    <div className="identity-row">
      <span className="identity-main">
        <span className="identity-name">
          <IdentityBadge name={ident.name} />
          <KindTag kind={ident.kind} />
          {/* 身分是每台一份（SPEC §16.2）：同名的 cc1 在別台是別的帳號，列上要看得出是哪一台。 */}
          <span className="host-count">{hostLabel(host)}</span>
          <span className="host-count">{used > 0 ? `${used} 個 Bot` : '未使用'}</span>
          <DisabledChip host={host} kind={ident.kind} name={ident.name} />
        </span>
        <span className="identity-detail" title={envText}>
          {envText ? envText.replace(/\n/g, ' ・ ') : '（無 env）'}
        </span>
        <IdentityHostLogins name={ident.name} kind={ident.kind} only={host} />
      </span>
      <button
        type="button"
        className="icon-btn"
        aria-label={`刪除身份 ${name}`}
        title={used > 0 ? '仍有 Bot 使用這個身份' : '刪除身份'}
        onClick={() => setConfirmDelete(true)}
      >
        ✕
      </button>
      <ConfirmDialog
        open={confirmDelete}
        title="刪除身份"
        body={
          <>
            要把<strong>{hostLabel(host)}</strong>的身份 <strong>{name}</strong> 移除嗎？別台同名的不受影響，已經登入的帳號本身也不受影響。
            {used > 0 ? (
              <>
                <br />
                <strong>{hostLabel(host)}仍有 {used} 個 Bot 綁著這個身份</strong>，需先把那些 Bot 改用別的身份或移除。
              </>
            ) : (
              ' 目前沒有 Bot 在用它。'
            )}
          </>
        }
        confirmLabel="刪除身份"
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
}

function NewIdentityForm() {
  const addIdentity = useStore((s) => s.addIdentity)
  const [name, setName] = useState('')
  const [kind, setKind] = useState<BotKind>('claude')
  const [envText, setEnvText] = useState(ENV_PREFILL.claude)
  const [busy, setBusy] = useState(false)
  const nameOk = /^[a-z][a-z0-9_-]{0,31}$/.test(name)

  return (
    <form
      className="form"
      onSubmit={(e) => {
        e.preventDefault()
        if (!nameOk || busy) return
        setBusy(true)
        void addIdentity({ name, kind, env: parseEnvText(envText) }).then((ok) => {
          setBusy(false)
          if (ok) setName('')
        })
      }}
    >
      <label className="field">
        <span>名稱（例如 cc1）</span>
        <input
          type="text"
          value={name}
          placeholder="cc1"
          spellCheck={false}
          onChange={(e) => setName(e.target.value.toLowerCase())}
        />
        {name && !nameOk ? (
          <span className="hint">小寫英文字母開頭，之後可接小寫字母、數字、- 或 _，最多 32 個字。</span>
        ) : null}
      </label>
      <label className="field">
        <span>kind</span>
        <select
          value={kind}
          onChange={(e) => {
            const next = e.target.value as BotKind
            // 使用者沒改過那一行才跟著換 kind 的設定目錄變數，免得蓋掉打好的。
            setEnvText((t) => (t === ENV_PREFILL[kind] ? ENV_PREFILL[next] : t))
            setKind(next)
          }}
        >
          <option value="claude">claude</option>
          <option value="codex">codex</option>
          <option value="grok">grok</option>
        </select>
      </label>
      <label className="field">
        <span>env（每行 KEY=VALUE；`$HOME` 與開頭 `~` 會以該主機的家目錄展開；claude 用 CLAUDE_CONFIG_DIR、grok 用 GROK_HOME）</span>
        <textarea rows={3} value={envText} spellCheck={false} onChange={(e) => setEnvText(e.target.value)} />
      </label>
      <div className="form-actions">
        <button type="submit" className="btn primary" disabled={!nameOk || busy}>
          新增身份
        </button>
      </div>
    </form>
  )
}

/** 從各主機 shell 認出來的 `ccN`（SPEC §16），唯讀；同名 config 身份會蓋過它，已列出的不重複。 */
function ShellIdentities() {
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
  collect('local', '本機', local)
  for (const h of hosts) collect(h.name, h.name, h.identity_status)
  if (rows.length === 0) return null
  return (
    <div className="identity-shell-block">
      <p className="hint">
        以下是從各主機登入 shell 的 <code>ccN</code> alias 認出來的身份（<code>~/.zshrc</code> 等），
        可以直接指派給 Bot；要改就改那台主機的 alias。
        「停用」只是把它從身份選單與自動挑選裡拿掉（alias 還在），已經在用它的 Bot 不受影響。
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
            <span className="identity-detail" title={st.config_dir ?? '預設帳號（無 CLAUDE_CONFIG_DIR）'}>
              {st.config_dir ? `CLAUDE_CONFIG_DIR=${st.config_dir}` : '（預設帳號）'}
            </span>
            {/* 同一種 chip 容器，否則單獨一顆會被 flex 拉成整行寬。 */}
            <span className="identity-logins">
              <span className="identity-login-wrap">
                <span
                  className={`identity-login is-${st.logged_in === true ? 'ok' : st.logged_in === false ? 'out' : 'unknown'}`}
                  title={st.logged_in === null ? st.reason ?? '登入狀態未知' : st.account ?? undefined}
                >
                  {label} {st.logged_in === true ? '已登入' : st.logged_in === false ? '未登入' : '未知'}
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
  const identities = useStore((s) => s.identities)
  return (
    <div className="hosts-panel identities-panel">
      {identities.length === 0 ? <p className="hint">尚未定義任何身份。</p> : null}
      {identities.map((i) => (
        <IdentityRow key={identityRowKey(i)} host={identityHost(i)} name={i.name} />
      ))}
      <ShellIdentities />
      <NewIdentityForm />
    </div>
  )
}
