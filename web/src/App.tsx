import { useEffect, useRef, useState } from 'react'
import { MOCK_MODE } from './api'
import { useDialogFocus } from './hooks/useDialogFocus'
import { useViewportPin } from './hooks/useViewportPin'
import { DRAWER_QUERY, useMediaQuery } from './hooks/useMediaQuery'
import { useProjectJumpKeys } from './hooks/useProjectJumpKeys'
import { ChatPanel } from './components/ChatPanel'
import { GroupChatPanel } from './components/GroupChatPanel'
import { HostShellPanel } from './components/HostShellPanel'
import { ImageShelf } from './components/ImageShelf'
import { MobilePreview } from './components/MobilePreview'
import { Sidebar } from './components/Sidebar'
import { screenTitle, useDrawerRoute } from './store/routeSync'
import { useStore } from './store/store'
import { totalUnread } from './store/unread'
import './components/relayedMessage.css'

/** 掛 `(N)` 前的原始標題；先剝掉既有 `(N)`，HMR／同址導覽下才不會疊成 `(1) (2) …`。 */
const BASE_TITLE = (typeof document === 'undefined' ? 'Agents Manager' : document.title)
  .replace(/^\(\d+\+?\)\s*/, '')
  // 也剝掉 `screenTitle` 的「畫面 · 」前綴，免得一路疊上去。
  .split(' · ')
  .slice(-1)[0]

/**
 * 選取沒變的點擊（再點回同一顆 bot）也要收抽屜。用 capture 是因為收合鍵與 `⋯` 會 stopPropagation；
 * 會在抽屜裡開對話框的按鈕要留著抽屜，否則 `inert` 會把對話框一起關掉。
 */
const DRAWER_NAV = '.bot-row, .project-head'
const DRAWER_STAY =
  '.bot-kids-toggle, .bot-actions, .bot-name-btn, .bot-name-input, .project-fold, .project-head-actions'

/** 分頁標題的 `(N)`，以及視窗回到前景時把開著的對話標為已讀（切走再回來才看到回覆的常見情境）。 */
function useUnread() {
  const total = useStore((s) => totalUnread(s.botUnread, s.hiddenBotIds))
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

function Notices() {
  const notices = useStore((s) => s.notices)
  const dismiss = useStore((s) => s.dismiss)
  if (notices.length === 0) return null
  return (
    <div className="notices" role="status" aria-live="polite">
      {notices.map((n) => (
        <div key={n.id} className={`notice ${n.kind}`}>
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
          <button type="button" onClick={() => dismiss(n.id)} aria-label="關閉">
            ✕
          </button>
        </div>
      ))}
    </div>
  )
}

function ConnBanner() {
  const connected = useStore((s) => s.connected)
  const socket = useStore((s) => s.socket)
  const stateStale = useStore((s) => s.stateStale)
  const refreshState = useStore((s) => s.refreshState)
  // socket 通、herdr 斷：daemon 之後會推狀態，但別乾等——每 3 秒（和回到分頁時）自己抓一次。
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
        ? '正在重新連線 daemon…'
        : '與 daemon 的連線中斷，正在重試…'
      : stateStale
        ? '狀態同步失敗，正在重試…'
      : 'daemon 與 herdr 連線中斷，暫時無法送出訊息'
  return (
    <div className="conn-banner" role="status">
      <span className={`conn-dot ${socket === 'open' && !connected ? 'closed' : socket}`} />
      <span>{label}</span>
      <button type="button" className="btn conn-retry" onClick={() => void refreshState()}>
        立即重試
      </button>
    </div>
  )
}

/** ⌥↑/⌥↓ switches bot from anywhere; plain ↑/↓ belongs to focus. Skipped inside a bot row (reorder) and dialogs. */
function useBotSwitchKeys() {
  const selectAdjacentBot = useStore((s) => s.selectAdjacentBot)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!e.altKey || e.metaKey || e.ctrlKey || e.isComposing) return
      if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return
      if (e.defaultPrevented) return
      const target = e.target instanceof Element ? e.target : null
      if (target?.closest('.bot-row')) return
      if (document.querySelector('.modal-backdrop, .confirm-backdrop')) return
      e.preventDefault()
      selectAdjacentBot(e.key === 'ArrowUp' ? -1 : 1)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [selectAdjacentBot])
}

export default function App() {
  const ready = useStore((s) => s.ready)
  const bootError = useStore((s) => s.bootError)
  const bootstrap = useStore((s) => s.bootstrap)
  const groupProjectId = useStore((s) => s.selectedProjectId)
  // 主機 shell 與群組互斥且優先：它是使用者剛按出來的暫時性視圖。
  const shellView = useStore((s) => s.shellView)
  const botId = useStore((s) => s.selectedBotId)
  // 設定面板開在主面板側，開了抽屜也要讓開。
  const settingsBotId = useStore((s) => s.settingsBotId)
  const [drawer, setDrawer] = useState(false)
  // Below this width the sidebar is an off-canvas drawer (styles.css `@media (width <= 1024px)`).
  const isMobile = useMediaQuery(DRAWER_QUERY)
  useViewportPin()
  const sidebarRef = useRef<HTMLElement>(null)
  const drawerOpen = isMobile && drawer

  useEffect(() => {
    void bootstrap()
  }, [bootstrap])

  useBotSwitchKeys()
  useProjectJumpKeys()
  useUnread()
  // 抽屜借一格歷史：開著時按上一頁是關抽屜，不是離開這個畫面。
  useDrawerRoute(drawerOpen, () => setDrawer(false))

  // Close the drawer when the selection or breakpoint changes. Compared during render, not in an
  // effect, to avoid painting the stale open drawer for a frame.
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
      // A dialog on top owns Escape; it is the drawer's turn only when none is open.
      if (document.querySelector('.modal-backdrop, .confirm-backdrop')) return
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
          <h1>{bootError ? '無法連上 daemon' : '連線中…'}</h1>
          {bootError ? (
            <>
              <p style={{ color: 'var(--danger)' }}>{bootError}</p>
              <p style={{ color: 'var(--text-dim)', fontSize: 13 }}>
                請確認 <code>agents-managerd serve</code> 正在 <code>127.0.0.1:7788</code> 執行，
                或改用 <code>VITE_MOCK=1 npm run dev</code> 以假資料開發。
              </p>
              <button type="button" className="btn primary" onClick={() => void bootstrap()}>
                重試
              </button>
            </>
          ) : (
            <p style={{ color: 'var(--text-dim)' }}>
              正在取得 <code>GET /api/session</code> 的 token…
              {MOCK_MODE ? '（MOCK 模式）' : ''}
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
        // Only at the drawer breakpoint while closed: offscreen, Tab and screen readers would still walk it.
        inert={isMobile && !drawer}
        {...(drawerOpen ? { role: 'dialog' as const, 'aria-modal': true, 'aria-label': '側邊欄' } : {})}
        onClickCapture={(e) => {
          if (!drawerOpen || !(e.target instanceof Element)) return
          if (!e.target.closest(DRAWER_NAV) || e.target.closest(DRAWER_STAY)) return
          setDrawer(false)
        }}
      >
        <Sidebar />
      </aside>
      {drawerOpen ? (
        <button type="button" className="scrim" aria-label="關閉側邊欄" onClick={() => setDrawer(false)} />
      ) : null}
      <main className="main">
        <ConnBanner />
        {/* 選著 bot 時 shell 是 ChatPanel 的分頁；group／沒選才整個換成 shell 面板。 */}
        {shellView && (groupProjectId || !botId) ? (
          <HostShellPanel
            key={`${shellView.host}:${shellView.paneId}`}
            host={shellView.host}
            paneId={shellView.paneId}
            cwd={shellView.cwd}
            onOpenSidebar={() => setDrawer(true)}
          />
        ) : groupProjectId ? (
          <GroupChatPanel key={groupProjectId} projectId={groupProjectId} onOpenSidebar={() => setDrawer(true)} />
        ) : (
          // key：換 bot 重新掛載，未送出的附件與捲動位置才不會跑到下一顆（草稿在 store，不受影響）。
          <ChatPanel key={botId ?? 'none'} onOpenSidebar={() => setDrawer(true)} />
        )}
      </main>
      {/* 刻意掛在 `main` 外：換 bot／project 不 unmount，暫存的檔案才跨得過去。 */}
      <ImageShelf />
      <MobilePreview />
      <Notices />
    </div>
  )
}
