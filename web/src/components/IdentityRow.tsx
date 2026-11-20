import { useRef, useState, type ReactNode } from 'react'
import { fmtDateTime, userTimeZone } from '../lib/userTime'
import { utcOffsetLabel } from '../lib/timeZone'
import type { BotKind } from '../api/types'
import { useT } from '../lib/i18n'
import { identityDisabled, useStore } from '../store/store'
import { DEFAULT_ACCOUNT, defaultAccountUseCount, findIdentity, hostLabel, identityUseCount } from '../store/identityRows'
import { loginKindOnHost } from '../lib/onboardingActions'
import { quotaForIdentity } from '../store/quotaLookup'
import { ConfirmDialog } from './ConfirmDialog'
import { KindTag } from './KindTag'
import { emptyEnvKeys, envDisplayText } from './identityEnv'
import './identitiesPanel.css'

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

/** 額度（5 小時窗）一眼可見：daemon 給的是「已用」百分比，這裡照樣顯示「已用」。沒有讀數就不標。 */
export function IdentityQuotaChip({ host, kind, identity }: { host: string; kind: BotKind; identity: string | null }) {
  const t = useT()
  const q = useStore((s) => quotaForIdentity(s.quota, host, kind, identity, s.identities))
  const w = q?.five_hour ?? null
  if (!w) return null
  return (
    <span className={`identity-quota${q?.stale ? ' is-stale' : ''}`} title={q?.stale ? t('上一輪讀數，尚未重新探測') : undefined}>
      {t('5h 已用 {pct}%', { pct: Math.round(w.used_pct) })}
    </span>
  )
}

/** 身份在每台主機各自的登入狀態（帳號是每台各自登入的）；`null` = 問不到，標「未知」而非「未登入」。 */
export function IdentityHostLogins({ name, kind, only, extraMore }: { name: string; kind?: string; only?: string; extraMore?: ReactNode }) {
  const t = useT()
  const localStatus = useStore((s) => s.localIdentityStatus[name])
  const localConnected = useStore((s) => s.connected)
  const allHosts = useStore((s) => s.hosts)
  // 明寫 host 的身份只屬於那一台：local 只看本機、遠端只看自己那台，別台同名的登入狀態是別的帳號。
  const rows: { host: string; label: string; state: boolean | null; account: string | null; reason: string | null; checkedAt: string | null | undefined; connected: boolean }[] = []
  if (!only || only === 'local') {
    rows.push({ host: 'local', label: t('本機'), state: localStatus?.logged_in ?? null, account: localStatus?.account ?? null, reason: localStatus?.reason ?? null, checkedAt: localStatus?.checked_at, connected: localConnected })
  }
  for (const h of only === undefined ? allHosts : only === 'local' ? [] : allHosts.filter((x) => x.name === only)) {
    const st = h.identity_status[name]
    rows.push({ host: h.name, label: h.name, state: h.connected ? (st?.logged_in ?? null) : null, account: st?.account ?? null, reason: st?.reason ?? null, checkedAt: st?.checked_at, connected: h.connected })
  }
  return (
    <span className="identity-logins">
      {rows.map((r) => {
        // 統一三態：已登入／未登入／無法確認；主機離線、最後檢查另外標。
        const cls = r.state === true ? 'ok' : r.state === false ? 'out' : 'unknown'
        const text = r.state === true ? t('已登入') : r.state === false ? t('未登入') : t('無法確認')
        const title =
          r.state === true
            ? r.account
              ? t('{label}：已登入（{account}）', { label: r.label, account: r.account })
              : t('{label}：已登入', { label: r.label })
            : r.state === false
              ? t('{label}：這個身份沒有登入，用它啟動的 bot 會停在登入畫面', { label: r.label })
              : t('{label}：無法確認（{reason}）', { label: r.label, reason: r.reason ?? t('尚未偵測過') })
        const more = r.connected && (r.state === true || kind) ? (
          <details className="identity-more">
            <summary>{t('更多')}</summary>
            <span className="identity-more-body">
              {r.state === true ? <IdentityLogoutButton host={r.host} identity={name} /> : null}
              {kind ? <IdentityDisableButton host={r.host} kind={kind} name={name} /> : null}
              {only === r.host ? extraMore : null}
            </span>
          </details>
        ) : null
        return (
          <span key={r.host} className="identity-login-wrap">
            <span className={`identity-login is-${cls}`} title={title}>
              {r.label} {text}
            </span>
            {r.state === true && r.account ? <span className="identity-account">{r.account}</span> : null}
            {!r.connected ? <span className="identity-offline">{t('主機離線')}</span> : null}
            <span className="identity-detail">
              {t('最後檢查')} {checkedAtText(r.checkedAt, t)}
            </span>
            {r.connected ? <IdentityLoginButton host={r.host} identity={name} loggedIn={r.state === true} /> : null}
            {more}
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

export function IdentityLoginButton({ host, identity, loggedIn }: { host: string; identity: string; loggedIn: boolean }) {
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
export function IdentityLogoutButton({ host, identity }: { host: string; identity: string }) {
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
export function IdentityDisableButton({ host, kind, name }: { host: string; kind: string; name: string }) {
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
export function DisabledChip({ host, kind, name }: { host: string; kind: string; name: string }) {
  const t = useT()
  const disabled = useStore((s) => identityDisabled(s, host, kind, name))
  if (!disabled) return null
  return (
    <span className="identity-disabled-chip" title={t('{name} 已停用：挑身份時不會出現，已經在用它的 Bot 不受影響', { name })}>
      {t('停用')}
    </span>
  )
}

export function IdentityRow({ host, name }: { host: string; name: string }) {
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
          <span className="host-count">{used > 0 ? t('{n} 個 Bot', { n: used }) : t('未使用')}</span>
          <DisabledChip host={host} kind={ident.kind} name={ident.name} />
        </span>
        {emptyKeys.length > 0 ? (
          <span className="identity-empty-env" role="alert" title={t('空值等於沒設；要改請到 config.toml 該身份的 [identities.env] 寫上值，或刪掉這個身份重建')}>
            {t('⚠ env {keys} 的值是空的，這個身份會悄悄用預設帳號', { keys: keyList })}
          </span>
        ) : null}
        <IdentityQuotaChip host={host} kind={ident.kind} identity={ident.name} />
        <IdentityHostLogins
          name={ident.name}
          kind={ident.kind}
          only={host}
          extraMore={
            <button
              type="button"
              className="icon-btn"
              aria-label={t('刪除身份 {name}', { name })}
              title={used > 0 ? t('仍有 Bot 使用這個身份') : t('刪除身份')}
              onClick={() => setConfirmDelete(true)}
            >
              ✕
            </button>
          }
        />
        <details className="identity-path">
          <summary>{t('路徑與 env')}</summary>
          <code>{envText || t('（無 env）')}</code>
        </details>
      </span>
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


/**
 * 每個 kind 的**預設帳號**（不換設定目錄的那個）：登入／登出／從 AG Man 隱藏（使用者 2026-11-14：「例如現在 grok 已不訂閱」）。
 * 登出＝該 CLI 自己的登出指令（claude／codex／grok 開臨時 pane，agy 刪憑證檔），不刪家目錄的檔案；
 * 隱藏＝額度條與這裡不再把它當可用帳號（`identity_prefs` 的 `@default`），可以隨時顯示回來，CLI 的登入不動。
 */
export function DefaultAccountRow({ host, kind }: { host: string; kind: BotKind }) {
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
              {where} {state === true ? t('已登入') : state === false ? t('未登入') : t('無法確認')}
            </span>
            <IdentityQuotaChip host={host} kind={kind} identity={null} />
            {!connected ? <span className="identity-offline">{t('主機離線')}</span> : null}
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
                <details className="identity-more">
                  <summary>{t('更多')}</summary>
                  <span className="identity-more-body">
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
                  </span>
                </details>
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

/** 上次檢查時間（使用者時區顯示，daemon 給的是 UTC）。沒問過就說沒問過。 */
function checkedAtText(iso: string | null | undefined, t: ReturnType<typeof useT>): string {
  if (!iso) return t('尚未檢查')
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const s = fmtDateTime(d, { hour12: false, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' })
  return `${s} (${utcOffsetLabel(userTimeZone())})`
}
