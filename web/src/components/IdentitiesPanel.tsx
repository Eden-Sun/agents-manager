import { useEffect, useRef, useState } from 'react'
import { useT } from '../lib/i18n'
import { BOT_KINDS, type BotKind } from '../api/types'
import { useStore } from '../store/store'
import { identityHost, identityRowKey } from '../store/identityRows'
import { useUnsavedGuard } from '../hooks/useUnsavedGuard'
import { DefaultAccountRow, IdentityRow } from './IdentityRow'
import { ShellIdentityRows } from './IdentityShellRows'
import { IdentityKindCard } from './IdentityKindCard'
import { IdentityHostTabs, IDENTITY_HOST_PANEL_ID, type HostTabItem } from './IdentityHostTabs'
import { AddIdentityDialog } from './AddIdentityDialog'
import { recheckIdentity } from '../api'
import './identitiesPanel.css'

export { IdentityBadge } from './IdentityRow'

const SWITCHABLE_KINDS: readonly BotKind[] = ['claude', 'codex', 'grok', 'agy']
const LAST_TAB_KEY = 'agm.identities.host-tab'
const FRESH_MS = 6000

function readLastTab(): string | null {
  try {
    return globalThis.localStorage?.getItem(LAST_TAB_KEY) ?? null
  } catch {
    return null
  }
}

function writeLastTab(name: string) {
  try {
    globalThis.localStorage?.setItem(LAST_TAB_KEY, name)
  } catch {
  }
}

export function IdentitiesPanel() {
  const t = useT()
  const hosts = useStore((s) => s.hosts)
  const identities = useStore((s) => s.identities)
  const localConnected = useStore((s) => s.connected)
  const localTools = useStore((s) => s.localTools)
  const localStatuses = useStore((s) => s.localIdentityStatus)
  const addIdentity = useStore((s) => s.addIdentity)
  const loginIdentity = useStore((s) => s.loginIdentity)
  const openIdentityLoginPane = useStore((s) => s.openIdentityLoginPane)
  const identityLoginPane = useStore((s) => s.identityLoginPane)
  const refreshState = useStore((s) => s.refreshState)
  const hostNames = ['local', ...hosts.map((h) => h.name)]
  const [picked, setPicked] = useState<string | null>(() => readLastTab())
  const active = picked && hostNames.includes(picked) ? picked : 'local'
  const [adding, setAdding] = useState<{ kind?: BotKind } | null>(null)
  const [fresh, setFresh] = useState<{ host: string; name: string } | null>(null)
  const listRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!fresh) return
    listRef.current?.querySelector('[data-fresh-row="true"]')?.scrollIntoView?.({ block: 'center' })
    const timer = setTimeout(() => setFresh(null), FRESH_MS)
    return () => clearTimeout(timer)
  }, [fresh])

  const summarize = (name: string, label: string, connected: boolean, tools: typeof localTools | undefined, statuses: typeof localStatuses): HostTabItem => {
    const defaults = BOT_KINDS.filter((k) => tools?.[k]?.installed && tools[k]?.logged_in === true).length
    const values = Object.values(statuses)
    return {
      name,
      label,
      connected,
      loggedIn: defaults + values.filter((st) => st.logged_in === true).length,
      anomaly: !connected || values.some((st) => Boolean(st.login_needed)),
    }
  }
  const tabs: HostTabItem[] = [
    summarize('local', t('本機'), localConnected, localTools, localStatuses),
    ...hosts.map((h) => summarize(h.name, h.name, h.connected, h.tools, h.identity_status)),
  ]
  const activeConnected = active === 'local' ? localConnected : hosts.find((h) => h.name === active)?.connected !== false
  const kinds: BotKind[] = [
    ...SWITCHABLE_KINDS,
    ...BOT_KINDS.filter((k) => !SWITCHABLE_KINDS.includes(k) && identities.some((i) => identityHost(i) === active && i.kind === k)),
  ]
  const choose = (name: string) => {
    setPicked(name)
    writeLastTab(name)
  }
  const freshLoggedIn = fresh
    ? fresh.host === 'local'
      ? (localStatuses[fresh.name]?.logged_in ?? null)
      : (hosts.find((h) => h.name === fresh.host)?.identity_status[fresh.name]?.logged_in ?? null)
    : null
  useUnsavedGuard(adding !== null)

  const dialogHosts = [
    { name: 'local', label: t('本機'), connected: localConnected },
    ...hosts.map((h) => ({ name: h.name, label: h.name, connected: h.connected })),
  ]

  return (
    <div className="hosts-panel identities-panel">
      <div className="identity-page-actions">
        <button type="button" className="btn primary" onClick={() => setAdding({})}>
          {t('新增帳號')}
        </button>
      </div>
      {hostNames.length > 1 ? <IdentityHostTabs tabs={tabs} value={active} onChange={choose} /> : null}
      <div role={hostNames.length > 1 ? 'tabpanel' : undefined} id={IDENTITY_HOST_PANEL_ID} aria-labelledby={hostNames.length > 1 ? `identity-tab-${active}` : undefined} className="identity-host-panel" ref={listRef}>
        {!activeConnected ? (
          <p className="identity-offline-banner" role="status">
            {t('主機離線：以下是最後已知的狀態，這台暫時不能登入、登出或變更身份。')}
          </p>
        ) : null}
        {fresh && fresh.host === active && freshLoggedIn !== true ? (
          <div className="identity-fresh-banner" role="status">
            <span>{t('已新增 {name}（尚未登入）', { name: fresh.name })}</span>
            {identityLoginPane?.host === fresh.host && identityLoginPane.identity === fresh.name ? (
              <button type="button" className="mini-btn" onClick={openIdentityLoginPane}>
                {t('打開登入終端')}
              </button>
            ) : null}
            <button
              type="button"
              className="mini-btn primary"
              onClick={() => {
                void loginIdentity(fresh.host, fresh.name, { background: true }).finally(() => void refreshState())
              }}
            >
              {t('立即登入')}
            </button>
          </div>
        ) : null}
        {kinds.map((kind) => (
          <IdentityKindCard key={kind} host={active} kind={kind} onAdd={() => setAdding({ kind })}>
            <DefaultAccountRow host={active} kind={kind} />
            {identities
              .filter((i) => identityHost(i) === active && i.kind === kind)
              .map((i) => {
                const isFresh = fresh !== null && fresh.host === active && fresh.name === i.name
                return (
                  <div key={identityRowKey(i)} className={isFresh ? 'identity-row-fresh' : undefined} data-fresh-row={isFresh ? 'true' : undefined}>
                    <IdentityRow host={active} name={i.name} />
                  </div>
                )
              })}
            <ShellIdentityRows host={active} kind={kind} />
          </IdentityKindCard>
        ))}
      </div>
      <AddIdentityDialog
        open={adding !== null}
        hosts={dialogHosts}
        initialHost={active}
        initialKind={adding?.kind}
        existing={identities}
        onCreate={(input) => addIdentity(input)}
        onLogin={(host, name) => loginIdentity(host, name, { background: true })}
        onOpenTerminal={openIdentityLoginPane}
        loggedIn={(host, name) =>
          host === 'local'
            ? (localStatuses[name]?.logged_in ?? null)
            : (hosts.find((h) => h.name === host)?.identity_status[name]?.logged_in ?? null)
        }
        refreshStatus={refreshState}
        recheck={(host, name) => recheckIdentity(host, name)}
        onCreated={(host, name) => {
          choose(host)
          setFresh({ host, name })
        }}
        onClose={() => setAdding(null)}
      />
    </div>
  )
}
