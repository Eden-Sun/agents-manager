import { fmtDateTime, userTimeZone } from '../lib/userTime'
import { utcOffsetLabel } from '../lib/timeZone'
import { useRef, useState } from 'react'
import type { BotKind, IdentityStatus, IdentityStatusMap } from '../api/types'
import { useT } from '../lib/i18n'
import { identityDisabled, useStore } from '../store/store'
import * as api from '../api'
import { DEFAULT_ACCOUNT, defaultAccountUseCount, findIdentity, hostLabel, identityHost, identityRowKey, identityUseCount, shadowedByConfig } from '../store/identityRows'
import { loginKindOnHost } from '../lib/onboardingActions'
import { BOT_KINDS } from '../api/types'
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
        const text = r.state === true ? t('已登入') : r.state === false ? t('未登入') : t('未知（尚未偵測）')
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

/** 各 kind 的設定目錄變數（SPEC §16／§12）：claude 用 CLAUDE_CONFIG_DIR、codex 用 CODEX_HOME、grok 用 GROK_HOME；agy 沒有，換 `HOME`。 */
const ENV_PREFILL: Record<BotKind, string> = {
  claude: 'CLAUDE_CONFIG_DIR=$HOME/.claude-',
  codex: 'CODEX_HOME=$HOME/.codex-',
  grok: 'GROK_HOME=$HOME/.grok-',
  // agy 的狀態在 `$HOME/.gemini`，沒有專用變數、XDG_* 也無效（2026-11-14 實測）：身份＝換一個 HOME（daemon 會建影子家目錄）。
  agy: 'HOME=$HOME/.agy-',
  'claude-api': '',
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
      <fieldset className="field kind-radios">
        <legend>kind</legend>
        <div className="kind-radio-choices" role="radiogroup" aria-label="kind">
          {(['claude', 'codex', 'grok', 'agy'] as const).map((k) => (
            <label key={k}>
              <input
                type="radio"
                name="identity-kind"
                value={k}
                checked={kind === k}
                onChange={() => {
                  // 使用者沒改過那一行才跟著換 kind 的設定目錄變數，免得蓋掉打好的。
                  setEnvText((cur) => (cur === ENV_PREFILL[kind] ? ENV_PREFILL[k] : cur))
                  setKind(k)
                }}
              />
              {k}
            </label>
          ))}
        </div>
        {kind === 'agy' ? (
          <span className="hint" data-k="agy-home-hint">
            {t.x('agy 沒有設定目錄變數，所以這個身份是換一個 {home}：daemon 會建那個目錄，{gemini} 與 agy 的憑證各自獨立（建好是未登入，登入請按身份列上的「登入」），其餘（git／ssh／PATH 等）用連結指回真家目錄。副作用：用這個身份的 agy 看到的 {homeVar} 是那個目錄。', { home: 'HOME', gemini: '.gemini', homeVar: '$HOME' })}
          </span>
        ) : null}
      </fieldset>
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
      // 只有同一台的 config 才蓋得掉：本機的 `cc1` 不能把 mac_builder 認出來的 `cc1`（別的帳號）藏起來。
      if (shadowedByConfig(configured, host, st.name)) continue
      rows.push({ host, label, st })
    }
  }
  collect('local', t('本機'), local)
  for (const h of hosts) collect(h.name, h.name, h.identity_status)
  if (rows.length === 0) return null
  return (
    <div className="identity-section identity-shell-block">
      <h3 className="identity-section-title">{t('Shell 偵測身份')}</h3>
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
                  title={[st.basis ?? st.reason ?? (st.logged_in === null ? t('登入狀態未知') : null), st.account ? `${t('帳號')}: ${st.account}` : null, `${t('上次檢查')}: ${checkedAtText(st.checked_at, t)}`].filter(Boolean).join('\n')}
                >
                  {label} {st.logged_in === true ? t('已登入') : st.logged_in === false ? t('未登入') : t('未知（無法判定）')}
                </span>
                <span className="identity-detail" title={st.basis ?? st.reason ?? undefined}>
                  {t('上次檢查')} {checkedAtText(st.checked_at, t)}
                  {st.logged_in === null && (st.reason ?? st.basis) ? ` · ${st.reason ?? st.basis}` : ''}
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

/**
 * 每個 kind 的**預設帳號**（不換設定目錄的那個）：登入／登出／從 AG Man 隱藏（使用者 2026-11-14：「例如現在 grok 已不訂閱」）。
 * 登出＝該 CLI 自己的登出指令（claude／codex／grok 開臨時 pane，agy 刪憑證檔），不刪家目錄的檔案；
 * 隱藏＝額度條與這裡不再把它當可用帳號（`identity_prefs` 的 `@default`），可以隨時顯示回來，CLI 的登入不動。
 */
function DefaultAccountRow({ host, kind }: { host: string; kind: BotKind }) {
  const t = useT()
  const tool = useStore((s) => (host === 'local' ? s.localTools : s.hosts.find((h) => h.name === host)?.tools)?.[kind])
  const connected = useStore((s) => host === 'local' || s.hosts.find((h) => h.name === host)?.connected === true)
  const used = useStore((s) => defaultAccountUseCount(s.bots, s.projects, host, kind))
  const hidden = useStore((s) => identityDisabled(s, host, kind, DEFAULT_ACCOUNT))
  const setIdentityDisabled = useStore((s) => s.setIdentityDisabled)
  const logout = useStore((s) => s.logoutKindDefault)
  const notify = useStore((s) => s.notify)
  const [confirmLogout, setConfirmLogout] = useState(false)
  const [confirmHide, setConfirmHide] = useState(false)
  const [busy, setBusy] = useState(false)
  const btnRef = useRef<HTMLButtonElement>(null)
  if (!tool?.installed) return null
  const state = tool.logged_in
  const where = hostLabel(host)
  const usedNote =
    used > 0 ? (
      <>
        <br />
        <strong>{t('{host}有 {n} 顆 Bot 沒指定身份、用的就是這個預設帳號', { host: where, n: used })}</strong>
        {t('：正在跑的不受影響，之後重新啟動會用別的帳號或停在登入畫面。')}
      </>
    ) : null
  return (
    <div className="identity-row is-default" data-k={`default-${host}-${kind}`}>
      <span className="identity-main">
        <span className="identity-name">
          <KindTag kind={kind} />
          <span className="host-count">{where}</span>
          <span className="host-count">{used > 0 ? t('{n} 個 Bot', { n: used }) : t('未使用')}</span>
          {hidden ? (
            <span className="identity-disabled-chip" title={t('已從 AG Man 隱藏：額度條不列它；CLI 的登入沒動')}>
              {t('已隱藏')}
            </span>
          ) : null}
        </span>
        <span className="identity-detail">{t('預設帳號（不指定身份的 Bot 用它）')}</span>
        <span className="identity-logins">
          <span className="identity-login-wrap">
            <span className={`identity-login is-${state === true ? 'ok' : state === false ? 'out' : 'unknown'}`}>
              {where} {state === true ? t('已登入') : state === false ? t('未登入') : t('未知')}
            </span>
            {connected ? (
              <>
                <button
                  type="button"
                  className="mini-btn identity-login-btn"
                  onClick={() => void loginKindOnHost(host, kind, null)}
                  title={t('在{host}開臨時終端登入預設帳號（不開瀏覽器）', { host: where })}
                >
                  {state === true ? t('重新登入') : t('登入')}
                </button>
                {state === true ? (
                  <button ref={btnRef} type="button" className="mini-btn identity-login-btn" disabled={busy} onClick={() => setConfirmLogout(true)}>
                    {busy ? t('登出中…') : t('登出')}
                  </button>
                ) : null}
                <button
                  type="button"
                  className={`mini-btn identity-login-btn${hidden ? ' is-off' : ''}`}
                  onClick={() => (hidden ? void setIdentityDisabled(host, kind, DEFAULT_ACCOUNT, false) : setConfirmHide(true))}
                  title={hidden ? t('顯示回來：額度條會再列出{host}的預設帳號', { host: where }) : t('隱藏：額度條不再列{host}的預設帳號（登入沒動，隨時可復原）', { host: where })}
                >
                  {hidden ? t('顯示') : t('隱藏')}
                </button>
              </>
            ) : null}
          </span>
        </span>
      </span>
      <ConfirmDialog
        open={confirmLogout}
        title={t('登出 {kind} 預設帳號', { kind })}
        body={
          <>
            {t.x('會在{host}開一個臨時 pane 執行 {kind} 自己的登出指令，清掉預設帳號的憑證；家目錄裡的其他檔案不會刪。', { host: where, kind: <code>{kind}</code> })}
            {usedNote}
          </>
        }
        confirmLabel={t('登出')}
        danger
        onCancel={() => setConfirmLogout(false)}
        onConfirm={() => {
          setConfirmLogout(false)
          setBusy(true)
          const btn = btnRef.current
          void logout(host, kind)
            .then((ok) => {
              if (ok && btn) closeEnclosingPopup(btn)
            })
            .catch((err) => notify('error', err instanceof Error ? err.message : String(err)))
            .finally(() => setBusy(false))
        }}
      />
      <ConfirmDialog
        open={confirmHide}
        title={t('隱藏 {kind} 預設帳號', { kind })}
        body={
          <>
            {t.x('隱藏之後{host}的額度條不再列 {kind} 的預設帳號。登入狀態與家目錄的檔案都不動，之後可以按「顯示」復原。', { host: where, kind: <code>{kind}</code> })}
            {usedNote}
          </>
        }
        confirmLabel={t('隱藏')}
        danger={used > 0}
        onCancel={() => setConfirmHide(false)}
        onConfirm={() => {
          setConfirmHide(false)
          void setIdentityDisabled(host, kind, DEFAULT_ACCOUNT, true)
        }}
      />
    </div>
  )
}

function DefaultAccounts() {
  const t = useT()
  const hosts = useStore((s) => s.hosts)
  const hostNames = ['local', ...hosts.map((h) => h.name)]
  return (
    <div className="identity-section identity-default-block">
      <h3 className="identity-section-title">{t('預設帳號')}</h3>
      <p className="hint">
        {t('每個 CLI 不換設定目錄的那個帳號。不訂閱了就「登出」（CLI 自己清憑證，不刪家目錄的檔案）並「隱藏」（額度條不再列它）；隱藏隨時可以顯示回來。')}
      </p>
      {hostNames.flatMap((host) => BOT_KINDS.map((kind) => <DefaultAccountRow key={`${host}:${kind}`} host={host} kind={kind} />))}
    </div>
  )
}

const SWITCHABLE_KINDS: readonly BotKind[] = ['claude', 'codex', 'grok', 'agy']

/** kind 啟用開關（本機）：停用＝不挑、不派、不啟動、不探測；已經在跑的 Bot 不殺。 */
function KindSwitches() {
  const t = useT()
  const disabledKinds = useStore((s) => s.disabledKinds)
  const setKindDisabled = useStore((s) => s.setKindDisabled)
  const busy = useStore((s) => Object.keys(s.busy).some((k) => k.startsWith('kind-disabled:')))
  return (
    <div className="identity-section">
      <h3 className="identity-section-title">{t('CLI 種類（本機）')}</h3>
      <p className="hint">{t('沒有訂閱的種類就停用：新 Bot 不再挑它，額度也不再探測；已經在跑的 Bot 不受影響，但重啟會被擋下。')}</p>
      {SWITCHABLE_KINDS.map((kind) => {
        const off = disabledKinds.includes(api.kindPrefKey('local', kind))
        return (
          <div key={kind} className="identity-row">
            <KindTag kind={kind} />
            {off ? <span className="identity-disabled-chip">{t('已停用')}</span> : null}
            <button
              type="button"
              className={`mini-btn identity-login-btn${off ? ' is-off' : ''}`}
              disabled={busy}
              onClick={() => void setKindDisabled('local', kind, !off)}
            >
              {off ? t('啟用') : t('停用')}
            </button>
          </div>
        )
      })}
    </div>
  )
}

/** 上次檢查時間（使用者時區顯示，daemon 給的是 UTC）。沒問過就說沒問過。 */
function checkedAtText(iso: string | null | undefined, t: ReturnType<typeof useT>): string {
  if (!iso) return t('尚未檢查')
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const s = fmtDateTime(d, { hour12: false, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' })
  return `${s} (${utcOffsetLabel(userTimeZone())})`
}

export function IdentitiesPanel() {
  const t = useT()
  const identities = useStore((s) => s.identities)
  return (
    <div className="hosts-panel identities-panel">
      <KindSwitches />
      <div className="identity-section">
        <h3 className="identity-section-title">{t('自訂身份')}</h3>
        {identities.length === 0 ? <p className="hint">{t('尚未定義任何身份。')}</p> : null}
        {identities.map((i) => (
          <IdentityRow key={identityRowKey(i)} host={identityHost(i)} name={i.name} />
        ))}
      </div>
      <ShellIdentities />
      <DefaultAccounts />
      <div className="identity-section">
        <h3 className="identity-section-title">{t('新增身份')}</h3>
        <NewIdentityForm />
      </div>
    </div>
  )
}
