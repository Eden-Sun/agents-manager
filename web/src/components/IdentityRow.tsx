import { useMemo, useRef, useState, type ReactNode } from 'react'
import { AGY_DEFAULT_IDENTITY, isBuiltinDefaultIdentity } from '../lib/defaultIdentity'
import { fmtDateTime, userTimeZone } from '../lib/userTime'
import { utcOffsetLabel } from '../lib/timeZone'
import type { Bot, BotKind } from '../api/types'
import { useT } from '../lib/i18n'
import { identityDisabled, useStore } from '../store/store'
import { DEFAULT_ACCOUNT, defaultAccountBots, effectiveIdentity, hostLabel, identityBots } from '../store/identityRows'
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
  if (kind === 'agy' && (name ? isBuiltinDefaultIdentity(kind, name) : showDefault)) {
    return (
      <span className="identity-badge" title={t('身份：ag0（agy 預設帳號）')}>
        {AGY_DEFAULT_IDENTITY}
      </span>
    )
  }
  if (!name) {
    if (!showDefault) return null
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

export function Cc0Note({ name, kind }: { name: string; kind: string }) {
  const t = useT()
  if (kind !== 'claude' || name !== 'cc0') return null
  const full = t('cc0 設定檔：有自己的 CLAUDE_CONFIG_DIR，只給指定 cc0 的 Bot 用；跟「預設帳號」是不同目錄，不會合併')
  return (
    <span className="identity-detail">
      {t('cc0 設定檔，與預設帳號不同目錄')}{' '}
      <span className="identity-info" tabIndex={0} title={full} aria-label={full}>
        ⓘ
      </span>
    </span>
  )
}

export function IdentityUsers({ bots }: { bots: readonly Pick<Bot, 'id' | 'name'>[] }) {
  const t = useT()
  if (bots.length === 0) return <span className="host-count">{t('沒有 Bot 使用')}</span>
  return (
    <details className="identity-users">
      <summary className="host-count">{t('{n} 個 Bot', { n: bots.length })}</summary>
      <ul className="identity-users-list">
        {bots.map((b) => (
          <li key={b.id}>
            <span className="identity-users-name">{b.name}</span>
            <button type="button" className="mini-btn" onClick={(e) => goRebind(e.currentTarget, b.id)}>
              {t('改綁…')}
            </button>
          </li>
        ))}
      </ul>
    </details>
  )
}

function goRebind(from: HTMLElement, botId: string) {
  closeEnclosingPopup(from)
  const s = useStore.getState()
  s.selectBot(botId)
  s.openSettings(botId)
}

export function IdentityMoreMenu({ general, danger }: { general?: ReactNode; danger?: ReactNode }) {
  const t = useT()
  if (!general && !danger) return null
  return (
    <details className="identity-more">
      <summary>{t('更多')}</summary>
      <span className="identity-more-body">
        {general ? (
          <span className="identity-menu-group">
            <span className="identity-menu-label">{t('一般')}</span>
            {general}
          </span>
        ) : null}
        {danger ? (
          <span className="identity-menu-group is-danger">
            <span className="identity-menu-label">{t('危險操作')}</span>
            {danger}
          </span>
        ) : null}
      </span>
    </details>
  )
}

export function IdentityHostLogins({ name, kind, only, danger }: { name: string; kind?: string; only?: string; danger?: ReactNode }) {
  const t = useT()
  const localStatus = useStore((s) => s.localIdentityStatus[name])
  const localConnected = useStore((s) => s.connected)
  const allHosts = useStore((s) => s.hosts)
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
        const menu = r.connected ? (
          <IdentityMoreMenu
            general={kind ? <IdentityDisableButton host={r.host} kind={kind} name={name} /> : undefined}
            danger={
              <>
                {r.state === true ? <IdentityLogoutButton host={r.host} identity={name} kind={kind ?? 'claude'} /> : null}
                {only === r.host ? danger : null}
              </>
            }
          />
        ) : null
        return (
          <span key={r.host} className="identity-login-wrap">
            <span className={`identity-login is-${cls}`} title={title}>
              {only === undefined ? `${r.label} ` : null}
              {text}
            </span>
            {r.state === true && r.account ? <span className="identity-account">{r.account}</span> : null}
            {!r.connected ? <span className="identity-offline">{t('主機離線')}</span> : null}
            <span className="identity-detail" title={checkedAtAbs(r.checkedAt)}>
              {checkedAtText(r.checkedAt, t)}
            </span>
            {r.connected ? <IdentityLoginButton host={r.host} identity={name} loggedIn={r.state === true} /> : null}
            {menu}
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
    <>
      <button
        type="button"
        className="mini-btn identity-login-btn identity-primary-btn"
        disabled={busy}
        title={t('在{host}開臨時 pane，帶著 {identity} 的設定執行登入（會在{host}開終端，不開瀏覽器）', { host: hostLabel(host), identity })}
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
        {busy ? t('登入中…') : loggedIn ? t('重新登入…') : t('登入')}
      </button>
    </>
  )
}

export function IdentityLogoutButton({ host, identity, kind }: { host: string; identity: string; kind: string }) {
  const t = useT()
  const logoutIdentity = useStore((s) => s.logoutIdentity)
  const notify = useStore((s) => s.notify)
  const bots = useStore((s) => s.bots)
  const projects = useStore((s) => s.projects)
  const used = useMemo(() => identityBots(bots, projects, host, identity).length, [bots, projects, host, identity])
  const [busy, setBusy] = useState(false)
  const [confirm, setConfirm] = useState(false)
  const btnRef = useRef<HTMLButtonElement>(null)
  return (
    <>
      <button
        ref={btnRef}
        type="button"
        className="mini-btn identity-login-btn identity-danger-btn"
        disabled={busy}
        title={t('在{host}開臨時 pane，帶著 {identity} 的設定登出這個帳號', { host: hostLabel(host), identity })}
        onClick={() => setConfirm(true)}
      >
        {busy ? t('登出中…') : t('登出此帳號…')}
      </button>
      <ConfirmDialog
        open={confirm}
        title={t('登出：{host} · {kind} · {identity}', { host: hostLabel(host), kind, identity })}
        body={
          <>
            {t.x('會在{host}開一個臨時 pane，帶著 {identity} 的設定目錄執行 {kind} 的登出，清掉這個身份的憑證。設定檔與家目錄其他內容不刪。', {
              host: hostLabel(host),
              identity: <strong>{identity}</strong>,
              kind: <code>{kind}</code>,
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
          ? t('重新提供：{name} 會再出現在{host}的身份選單與自動挑選裡', { name, host: hostLabel(host) })
          : t('不再提供給新 Bot：{name} 不再出現在{host}的身份選單與自動挑選裡，已經在用它的 Bot 不受影響', { name, host: hostLabel(host) })
      }
      onClick={() => void setIdentityDisabled(host, kind, name, !disabled)}
    >
      {disabled ? t('重新提供給新 Bot') : t('不再提供給新 Bot')}
    </button>
  )
}

export function DisabledChip({ host, kind, name }: { host: string; kind: string; name: string }) {
  const t = useT()
  const disabled = useStore((s) => identityDisabled(s, host, kind, name))
  if (!disabled) return null
  return (
    <span className="identity-disabled-chip" title={t('{name} 不再提供給新 Bot：挑身份時不會出現，已經在用它的 Bot 不受影響', { name })}>
      {t('不提供新 Bot')}
    </span>
  )
}

export function IdentityRow({ host, name }: { host: string; name: string }) {
  const t = useT()
  const ident = useStore((s) => effectiveIdentity(s.identities, host, name))
  const bots = useStore((s) => s.bots)
  const projects = useStore((s) => s.projects)
  const usedBots = useMemo(() => identityBots(bots, projects, host, name), [bots, projects, host, name])
  const used = usedBots.length
  const removeIdentity = useStore((s) => s.removeIdentity)
  const [confirmDelete, setConfirmDelete] = useState(false)
  if (!ident) return null
  const isSharedOnRemote = host !== 'local' && !ident.host
  const envText = envDisplayText(ident.env)
  const emptyKeys = emptyEnvKeys(ident.env)
  const keyList = emptyKeys.join(t('、'))
  const where = hostLabel(host)
  return (
    <div className="identity-row">
      <span className="identity-main">
        <span className="identity-name">
          <IdentityBadge name={ident.name} />
          {isSharedOnRemote ? <span className="identity-source-tag">{t('共用')}</span> : null}
          <IdentityUsers bots={usedBots} />
          <DisabledChip host={host} kind={ident.kind} name={ident.name} />
        </span>
        <Cc0Note name={ident.name} kind={ident.kind} />
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
          danger={
            !isSharedOnRemote ? (
              <button
                type="button"
                className="mini-btn identity-login-btn identity-danger-btn"
                onClick={() => setConfirmDelete(true)}
                title={t('只移除設定檔裡的這個身份；登入憑證與家目錄保留')}
              >
                {t('移除身份設定…')}
              </button>
            ) : undefined
          }
        />
        <details className="identity-path">
          <summary>{t('路徑與 env')}</summary>
          <code>{envText || t('（無 env）')}</code>
        </details>
      </span>
      <ConfirmDialog
        open={confirmDelete}
        title={t('移除身份設定：{host} · {kind} · {name}', { host: where, kind: ident.kind, name })}
        body={
          <>
            {t.x('會從{host}的設定檔移除身份 {name}。別台同名的不受影響，已經登入的帳號本身（憑證與家目錄）也不受影響。', {
              host: <strong>{where}</strong>,
              name: <strong>{name}</strong>,
            })}
            {used > 0 ? (
              <>
                <br />
                <strong>{t('仍有 {n} 個 Bot 綁著這個身份', { n: used })}</strong>
                {t('：先到上方「{n} 個 Bot」逐一改綁到別的身份，才能移除。', { n: used })}
              </>
            ) : (
              t(' 目前沒有 Bot 在用它。')
            )}
          </>
        }
        confirmLabel={t('移除身份設定')}
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

export function DefaultAccountRow({ host, kind }: { host: string; kind: BotKind }) {
  const t = useT()
  const tool = useStore((s) => (host === 'local' ? s.localTools : s.hosts.find((h) => h.name === host)?.tools)?.[kind])
  const connected = useStore((s) => host === 'local' || s.hosts.find((h) => h.name === host)?.connected === true)
  const bots = useStore((s) => s.bots)
  const projects = useStore((s) => s.projects)
  const usedBots = useMemo(() => defaultAccountBots(bots, projects, host, kind), [bots, projects, host, kind])
  const used = usedBots.length
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
          <IdentityUsers bots={usedBots} />
          {hidden ? (
            <span className="identity-disabled-chip" title={t('已從額度列隱藏：額度列不列它；CLI 的登入沒動')}>
              {t('已隱藏')}
            </span>
          ) : null}
        </span>
        <span className="identity-detail">
          {kind === 'agy' ? t('ag0：預設帳號（未指定身份或指定 ag0 的 Bot 使用）') : t('預設帳號（未指定身份的 Bot 使用；不是 cc0 設定檔）')}
        </span>
        <span className="identity-logins">
          <span className="identity-login-wrap">
            <span className={`identity-login is-${state === true ? 'ok' : state === false ? 'out' : 'unknown'}`}>
              {state === true ? t('已登入') : state === false ? t('未登入') : t('無法確認')}
            </span>
            <IdentityQuotaChip host={host} kind={kind} identity={null} />
            {!connected ? <span className="identity-offline">{t('主機離線')}</span> : null}
            {connected ? (
              <>
                <button
                  type="button"
                  className="mini-btn identity-login-btn identity-primary-btn"
                  onClick={() => void loginKindOnHost(host, kind, null)}
                  title={t('在{host}開臨時終端登入預設帳號（會在{host}開終端，不開瀏覽器）', { host: where })}
                >
                  {state === true ? t('重新登入…') : t('登入')}
                </button>
                <IdentityMoreMenu
                  general={
                    <button
                      type="button"
                      className={`mini-btn identity-login-btn${hidden ? ' is-off' : ''}`}
                      onClick={() => (hidden ? void setIdentityDisabled(host, kind, DEFAULT_ACCOUNT, false) : setConfirmHide(true))}
                      title={hidden ? t('顯示回額度列：額度列會再列出{host}的預設帳號', { host: where }) : t('從額度列隱藏：額度列不再列{host}的預設帳號（登入沒動，隨時可復原）', { host: where })}
                    >
                      {hidden ? t('顯示回額度列') : t('從額度列隱藏')}
                    </button>
                  }
                  danger={
                    state === true ? (
                      <button ref={btnRef} type="button" className="mini-btn identity-login-btn identity-danger-btn" disabled={busy} onClick={() => setConfirmLogout(true)}>
                        {busy ? t('登出中…') : t('登出此帳號…')}
                      </button>
                    ) : undefined
                  }
                />
              </>
            ) : null}
          </span>
        </span>
      </span>
      <ConfirmDialog
        open={confirmLogout}
        title={t('登出：{host} · {kind} · 預設帳號', { host: where, kind })}
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
        title={t('從額度列隱藏：{host} · {kind} · 預設帳號', { host: where, kind })}
        body={t.x('只影響{host}的額度列：不再列出 {kind} 預設帳號的剩餘額度。登入狀態與家目錄都不動，之後可以按「顯示回額度列」復原。', { host: where, kind: <code>{kind}</code> })}
        confirmLabel={t('從額度列隱藏')}
        onCancel={() => setConfirmHide(false)}
        onConfirm={() => {
          setConfirmHide(false)
          void setIdentityDisabled(host, kind, DEFAULT_ACCOUNT, true)
        }}
      />
    </div>
  )
}

function checkedAtText(iso: string | null | undefined, t: ReturnType<typeof useT>): string {
  if (!iso) return t('尚未檢查')
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const mins = Math.floor((Date.now() - d.getTime()) / 60_000)
  if (mins < 1) return t('剛剛檢查')
  if (mins < 60) return t('{n} 分鐘前檢查', { n: mins })
  const hours = Math.floor(mins / 60)
  if (hours < 24) return t('{n} 小時前檢查', { n: hours })
  const days = Math.floor(hours / 24)
  if (days < 7) return t('{n} 天前檢查', { n: days })
  return `${t('檢查於')} ${fmtDateTime(d, { hour12: false, year: 'numeric', month: '2-digit', day: '2-digit' })}`
}

function checkedAtAbs(iso: string | null | undefined): string | undefined {
  if (!iso) return undefined
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const s = fmtDateTime(d, { hour12: false, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' })
  return `${s} (${utcOffsetLabel(userTimeZone())})`
}
