import { AgmDeleteConfirm } from './components/AgmDeleteConfirm'
import { ResetCreditOffer } from './components/ResetCreditOffer'
import { useEffect, useRef, useState } from 'react'
import { MOCK_MODE } from './api'
import { useDialogFocus } from './hooks/useDialogFocus'
import { useViewportPin } from './hooks/useViewportPin'
import { useWakeLockHolder } from './hooks/useWakeLock'
import { useFileDropGuard } from './hooks/useFileDropGuard'
import { DRAWER_QUERY, useMediaQuery } from './hooks/useMediaQuery'
import { useProjectJumpKeys } from './hooks/useProjectJumpKeys'
import { useBotSwitchKeys } from './hooks/useBotSwitchKeys'
import { dialogOpen } from './lib/dialogOpen'
import { ChatPanel } from './components/ChatPanel'
import { HostOfflineBanner } from './components/HostOffline'
import { LoginPromptBanner } from './components/LoginPromptBanner'
import { CodexLoginReplaceDialog } from './components/CodexLoginReplaceDialog'
import { ApiModelConflictDialog } from './components/ApiModelConflictDialog'
import { OnboardingBanner } from './components/OnboardingChecklist'
import { ProjectSettingsPanel } from './components/ProjectSettingsPanel'
import { HostShellPanel } from './components/HostShellPanel'
import { ImageShelf } from './components/ImageShelf'
import { MobilePreview } from './components/MobilePreview'
import { PreviewColumn } from './components/PreviewColumn'
import { Sidebar } from './components/Sidebar'
import { screenTitle, useDrawerRoute } from './store/routeSync'
import { useStore } from './store/store'
import { useBlockedPrefetch } from './hooks/useBlockedPrefetch'
import { titleUnread } from './store/unread'
import './components/relayedMessage.css'
import { useT } from './lib/i18n'
import { installPresenceReporter, installPushClickBridge, parseResetOfferUrl } from './lib/pushNotify'
import { reopenResetCreditOffer } from './store/resetCreditOffer'
import { drawerShouldClose } from './lib/drawerNav'
import { StaleBundleBanner } from './components/StaleBundleBanner'

const BASE_TITLE = (typeof document === 'undefined' ? 'Agents Manager' : document.title)
  .replace(/^\(\d+\+?\)\s*/, '')
  .split(' · ')
  .slice(-1)[0]

function usePushClickBridge() {
  useEffect(() => installPushClickBridge(), [])
  useEffect(() => {
    const onOpen = (ev: Event) => useStore.getState().selectBot((ev as CustomEvent<{ botId: string }>).detail.botId)
    window.addEventListener('agm:open-bot', onOpen)
    const onOffer = (ev: Event) => reopenResetCreditOffer((ev as CustomEvent<{ key: string }>).detail.key)
    window.addEventListener('agm:open-reset-offer', onOffer)
    const initial = parseResetOfferUrl(window.location.href)
    if (initial) {
      reopenResetCreditOffer(initial)
      try {
        window.history.replaceState(window.history.state, '', window.location.pathname)
      } catch {
      }
    }
    const stop = installPresenceReporter(() => useStore.getState().selectedBotId)
    return () => {
      window.removeEventListener('agm:open-bot', onOpen)
      window.removeEventListener('agm:open-reset-offer', onOffer)
      stop()
    }
  }, [])
}

function useUnread() {
  const total = useStore(titleUnread)
  const screen = useStore(screenTitle)
  const markCurrentRead = useStore((s) => s.markCurrentRead)
  useEffect(() => {
    const base = screen ? `${screen} · ${BASE_TITLE}` : BASE_TITLE
    document.title = total > 0 ? `(${total > 99 ? '99+' : total}) ${base}` : base
  }, [total, screen])
  useEffect(() => {
    const seen = () => markCurrentRead()
    window.addEventListener('focus', seen)
    document.addEventListener('visibilitychange', seen)
    return () => {
      window.removeEventListener('focus', seen)
      document.removeEventListener('visibilitychange', seen)
    }
  }, [markCurrentRead])
}

import { collectClaudeExpiryAlerts, formatExpiryTime } from './lib/claudeLoginExpiry'
export function Notices() {
  const t = useT()
  const notices = useStore((s) => s.notices)
  const dismiss = useStore((s) => s.dismiss)
  const localClaudeLoginExpiresAt = useStore((s) => s.localClaudeLoginExpiresAt)
  const hosts = useStore((s) => s.hosts)
  const dismissedLoginExpiry = useStore((s) => s.dismissedLoginExpiry)
  const dismissLoginExpiry = useStore((s) => s.dismissLoginExpiry)
  const loginIdentity = useStore((s) => s.loginIdentity)
  const expiryAlerts = collectClaudeExpiryAlerts({
    localExpiry: localClaudeLoginExpiresAt,
    hosts,
    nowMs: Date.now(),
  }).filter((a) => dismissedLoginExpiry[`${a.host}:${a.account}`] !== a.stage)
  return (
    <div className="notices" role="status" aria-live="polite">
      {expiryAlerts.map((a) => {
        const key = `${a.host}:${a.account}`
        const hostName = a.host === 'local' ? t('本機') : a.host
        const noticeText =
          a.stage === 'expired'
            ? t('{account}（{host}）的 Claude 登入已過期', { account: a.account, host: hostName })
            : t('{account}（{host}）的 Claude 登入將於 {time} 到期', {
                account: a.account,
                host: hostName,
                time: formatExpiryTime(a.expiresAtMs),
              })
        return (
          <div
            key={`expiry:${key}`}
            className={`notice ${a.stage === 'expired' ? 'error' : 'warn'}`}
            role={a.stage === 'expired' ? 'alert' : undefined}
          >
            <span style={{ flex: 1 }}>{noticeText}</span>
            <button
              type="button"
              className="notice-action"
              onClick={() => {
                void loginIdentity(a.host, a.account)
              }}
            >
              {t('重新登入')}
            </button>
            <button type="button" onClick={() => dismissLoginExpiry(key, a.stage)} aria-label={t('關閉')}>
              ✕
            </button>
          </div>
        )
      })}
      {notices.map((n) => (
        <div key={n.id} className={`notice ${n.kind}${n.topCenter ? ' notice-top-center' : ''}`} role={n.kind === 'error' ? 'alert' : undefined}>
          <span style={{ flex: 1 }}>{n.text}</span>
          {n.action ? (
            <button
              type="button"
              className="notice-action"
              onClick={() => {
                void n.action?.run()
                dismiss(n.id)
              }}
            >
              {n.action.label}
            </button>
          ) : null}
          <button type="button" onClick={() => dismiss(n.id)} aria-label={t('關閉')}>
            ✕
          </button>
        </div>
      ))}
    </div>
  )
}

function ConnBanner() {
  const t = useT()
  const connected = useStore((s) => s.connected)
  const socket = useStore((s) => s.socket)
  const stateStale = useStore((s) => s.stateStale)
  const refreshState = useStore((s) => s.refreshState)
  const herdrDown = socket === 'open' && !connected
  const shouldRetry = herdrDown || stateStale
  useEffect(() => {
    if (!shouldRetry) return
    const tick = () => void refreshState()
    const id = setInterval(tick, 3_000)
    window.addEventListener('focus', tick)
    return () => {
      clearInterval(id)
      window.removeEventListener('focus', tick)
    }
  }, [refreshState, shouldRetry])
  if (socket === 'open' && connected && !stateStale) return null
  const label =
    socket !== 'open'
      ? socket === 'connecting'
        ? t('正在重新連線 daemon…')
        : t('與 daemon 的連線中斷，正在重試…')
      : stateStale
        ? t('狀態同步失敗，正在重試…')
      : t('daemon 與 herdr 連線中斷，暫時無法送出訊息')
  return (
    <div className="conn-banner" role="status">
      <span className={`conn-dot ${socket === 'open' && !connected ? 'closed' : socket}`} />
      <span>{label}</span>
      <button type="button" className="btn conn-retry" onClick={() => void refreshState()}>
        {t('立即重試')}
      </button>
    </div>
  )
}

export default function App() {
  const t = useT()
  const ready = useStore((s) => s.ready)
  const bootError = useStore((s) => s.bootError)
  const bootstrap = useStore((s) => s.bootstrap)
  const groupProjectId = useStore((s) => s.selectedProjectId)
  const shellView = useStore((s) => s.shellView)
  const botId = useStore((s) => s.selectedBotId)
  const settingsBotId = useStore((s) => s.settingsBotId)
  const [drawer, setDrawer] = useState(false)
  const isMobile = useMediaQuery(DRAWER_QUERY)
  useViewportPin()
  useWakeLockHolder()
  useFileDropGuard()
  const sidebarRef = useRef<HTMLElement>(null)
  const drawerOpen = isMobile && drawer

  useEffect(() => {
    void bootstrap()
  }, [bootstrap])

  useBotSwitchKeys()
  useProjectJumpKeys()
  useUnread()
  usePushClickBridge()
  useBlockedPrefetch()
  useDrawerRoute(drawerOpen, () => setDrawer(false))

  const selection = `${botId ?? ''}|${groupProjectId ?? ''}|${shellView ? `${shellView.host}:${shellView.paneId}` : ''}|${settingsBotId ?? ''}`
  const [lastNav, setLastNav] = useState({ selection, isMobile })
  if (lastNav.selection !== selection || lastNav.isMobile !== isMobile) {
    setLastNav({ selection, isMobile })
    if (drawer) setDrawer(false)
  }

  useEffect(() => {
    if (!drawerOpen) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || e.defaultPrevented) return
      if (dialogOpen()) return
      e.preventDefault()
      setDrawer(false)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [drawerOpen])

  useDialogFocus(drawerOpen, sidebarRef)

  if (!ready) {
    return (
      <div className="boot">
        <div className="boot-card">
          <h1>{bootError ? t('無法連上 daemon') : t('連線中…')}</h1>
          {bootError ? (
            <>
              <p style={{ color: 'var(--danger)' }}>{bootError}</p>
              <p style={{ color: 'var(--text-dim)', fontSize: 13 }}>
                {t.x('請確認 {serve} 正在 {addr} 執行，或改用 {mock} 以假資料開發。', {
                  serve: <code>agents-managerd serve</code>,
                  addr: <code>127.0.0.1:7788</code>,
                  mock: <code>VITE_MOCK=1 npm run dev</code>,
                })}
              </p>
              <button type="button" className="btn primary" onClick={() => void bootstrap()}>
                {t('重試')}
              </button>
            </>
          ) : (
            <p style={{ color: 'var(--text-dim)' }}>
              {t.x('正在取得 {code} 的 token…', { code: <code>GET /api/session</code> })}
              {MOCK_MODE ? t('（MOCK 模式）') : ''}
            </p>
          )}
        </div>
      </div>
    )
  }

  return (
    <div className="app">
      <aside
        ref={sidebarRef}
        className={`sidebar${drawer ? ' open' : ''}`}
        inert={isMobile && !drawer}
        {...(drawerOpen ? { role: 'dialog' as const, 'aria-modal': true, 'aria-label': t('側邊欄') } : {})}
        onClickCapture={(e) => {
          if (!drawerOpen || !drawerShouldClose(e.target)) return
          setDrawer(false)
        }}
      >
        <Sidebar />
      </aside>
      {drawerOpen ? (
        <button type="button" className="scrim" aria-label={t('關閉側邊欄')} onClick={() => setDrawer(false)} />
      ) : null}
      <main className="main">
        <ConnBanner />
        <HostOfflineBanner />
        <LoginPromptBanner />
        <CodexLoginReplaceDialog />
        <ApiModelConflictDialog />
        <OnboardingBanner compact={Boolean(botId || groupProjectId || shellView)} />
        {}
        {shellView && (groupProjectId || !botId) ? (
          <HostShellPanel
            key={`${shellView.host}:${shellView.paneId}`}
            host={shellView.host}
            paneId={shellView.paneId}
            cwd={shellView.cwd}
            onOpenSidebar={() => setDrawer(true)}
          />
        ) : groupProjectId ? (
          <ProjectSettingsPanel key={groupProjectId} projectId={groupProjectId} onOpenSidebar={() => setDrawer(true)} />
        ) : (
          <ChatPanel key={botId ?? 'none'} onOpenSidebar={() => setDrawer(true)} />
        )}
      </main>
      {}
      <ImageShelf />
      <MobilePreview />
      <PreviewColumn />
      <Notices />
      <StaleBundleBanner />
      <AgmDeleteConfirm />
      <ResetCreditOffer />
    </div>
  )
}
