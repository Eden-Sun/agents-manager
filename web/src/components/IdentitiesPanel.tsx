import { useState } from 'react'
import type { BotKind, IdentityStatus, IdentityStatusMap } from '../api/types'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { KindTag } from './KindTag'

/**
 * 身份預設（identities）：例如 `cc1` = 用另一個 `CLAUDE_CONFIG_DIR` 跑不同帳號。
 * daemon 啟動 bot 時把 `env` 注入 pane。身份的本體就是 env，所以表單只收 env；
 * `args` 契約仍在（舊設定讀得到），但 UI 不再提供輸入。
 */

/** `KEY=VALUE` 每行 → map；忽略空行與 `#` 註解。 */
export function parseEnvText(text: string): Record<string, string> {
  const out: Record<string, string> = {}
  for (const raw of text.split('\n')) {
    const line = raw.trim()
    if (!line || line.startsWith('#')) continue
    const i = line.indexOf('=')
    if (i <= 0) continue
    out[line.slice(0, i).trim()] = line.slice(i + 1).trim()
  }
  return out
}

export function envToText(env: Record<string, string>): string {
  return Object.entries(env)
    .map(([k, v]) => `${k}=${v}`)
    .join('\n')
}

export function IdentityBadge({
  name,
  showDefault,
  kind,
}: {
  name: string | null
  showDefault?: boolean
  kind?: BotKind
}) {
  // 沒指定身份時仍要標出來，不然本機預設和 cc1 在列表上長得一樣。
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

/**
 * 一個身份在每台主機上的登入狀態。
 *
 * 身份是全域設定，但它指到的帳號是**每台主機各自登入**的：`CLAUDE_CONFIG_DIR` 在每台機器
 * 都展得開，那個資料夾裡卻不一定有能用的帳號。所以這裡一台一台列，而不是給一個總結。
 * `null` = 問不到（CLI 沒裝、還沒偵測），標「未知」而不是「未登入」。
 */
function IdentityHostLogins({ name }: { name: string }) {
  const localStatus = useStore((s) => s.localIdentityStatus[name])
  const localConnected = useStore((s) => s.connected)
  const hosts = useStore((s) => s.hosts)
  const rows: { host: string; label: string; state: boolean | null; account: string | null; reason: string | null; connected: boolean }[] = [
    { host: 'local', label: '本機', state: localStatus?.logged_in ?? null, account: localStatus?.account ?? null, reason: localStatus?.reason ?? null, connected: localConnected },
  ]
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
            {r.connected ? <IdentityLoginButton host={r.host} identity={name} loggedIn={r.state === true} /> : null}
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
      title={`在${host === 'local' ? '本機' : host}開臨時 pane，帶著 ${identity} 的設定執行登入`}
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

function IdentityRow({ name }: { name: string }) {
  const ident = useStore((s) => s.identities.find((i) => i.name === name))
  const used = useStore((s) => s.bots.filter((b) => b.identity === name).length)
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
          <span className="host-count">{used > 0 ? `${used} 個 Bot` : '未使用'}</span>
        </span>
        <span className="identity-detail" title={envText}>
          {envText ? envText.replace(/\n/g, ' ・ ') : '（無 env）'}
        </span>
        <IdentityHostLogins name={ident.name} />
      </span>
      <button
        type="button"
        className="icon-btn"
        title={used > 0 ? '仍有 Bot 使用這個身份' : '刪除身份'}
        onClick={() => setConfirmDelete(true)}
      >
        ✕
      </button>
      {/* 本來是 `window.confirm()`；跟主機、Project 一起換成同一顆確認框。 */}
      <ConfirmDialog
        open={confirmDelete}
        title="刪除身份"
        body={
          <>
            要把身份 <strong>{name}</strong> 移除嗎？已經登入的帳號本身不受影響。
            {used > 0 ? (
              <>
                <br />
                <strong>仍有 {used} 個 Bot 綁著這個身份</strong>，需先把那些 Bot 改用別的身份或移除。
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
          void removeIdentity(name)
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
        {/* 同 `HostsPanel`：不要把 regex 印給使用者看。 */}
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
            // 預填的是各 kind 自己的設定目錄變數；使用者沒改過那一行才跟著換，免得把打好的蓋掉。
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

/**
 * 從各主機 shell 認出來的 `ccN`（SPEC §16）。唯讀：它們是使用者 zshrc 裡的 alias，
 * 這裡只負責讓人看見「daemon 認到了什麼、指到哪個設定目錄、登入了沒」。
 * 同名的 config 身份會蓋過它，所以已經在上面列出來的就不重複列。
 */
function ShellIdentities() {
  const configured = useStore((s) => s.identities)
  const local = useStore((s) => s.localIdentityStatus)
  const hosts = useStore((s) => s.hosts)
  const rows: { host: string; label: string; st: IdentityStatus }[] = []
  const collect = (host: string, label: string, map: IdentityStatusMap) => {
    for (const st of Object.values(map)) {
      if (st.source !== 'shell') continue
      if (configured.some((i) => i.name === st.name)) continue
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
      </p>
      {rows.map(({ host, label, st }) => (
        <div className="identity-row is-shell" key={`${host}:${st.name}`}>
          <span className="identity-main">
            <span className="identity-name">
              <IdentityBadge name={st.name} />
              <KindTag kind={st.kind} />
              <span className="host-count">{label}</span>
            </span>
            <span className="identity-detail" title={st.config_dir ?? '預設帳號（無 CLAUDE_CONFIG_DIR）'}>
              {st.config_dir ? `CLAUDE_CONFIG_DIR=${st.config_dir}` : '（預設帳號）'}
            </span>
            {/* 和 config 那些列同一種 chip 容器，否則單獨一顆會被 flex 拉成整行寬。 */}
            <span className="identity-logins">
              <span className="identity-login-wrap">
                <span
                  className={`identity-login is-${st.logged_in === true ? 'ok' : st.logged_in === false ? 'out' : 'unknown'}`}
                  title={st.logged_in === null ? st.reason ?? '登入狀態未知' : st.account ?? undefined}
                >
                  {label} {st.logged_in === true ? '已登入' : st.logged_in === false ? '未登入' : '未知'}
                </span>
                <IdentityLoginButton host={host} identity={st.name} loggedIn={st.logged_in === true} />
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
        <IdentityRow key={i.name} name={i.name} />
      ))}
      <ShellIdentities />
      <NewIdentityForm />
    </div>
  )
}
