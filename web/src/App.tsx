import { useEffect, useRef, useState } from 'react'
import { MOCK_MODE } from './api'
import { useDialogFocus } from './hooks/useDialogFocus'
import { useViewportPin } from './hooks/useViewportPin'
import { DRAWER_QUERY, useMediaQuery } from './hooks/useMediaQuery'
import { ChatPanel } from './components/ChatPanel'
import { GroupChatPanel } from './components/GroupChatPanel'
import { HostShellPanel } from './components/HostShellPanel'
import { ImageShelf } from './components/ImageShelf'
import { Sidebar } from './components/Sidebar'
import { TeamLaunchPanel } from './components/TeamLaunchPanel'
import { TeamPanel } from './components/TeamPanel'
import { screenTitle, useDrawerRoute } from './store/routeSync'
import { useStore } from './store/store'
import { totalUnread } from './store/unread'
import './components/relayedMessage.css'

/**
 * 掛 `(N)` 之前的原始標題。要把既有的 `(N)` 剝掉再存：模組不一定在乾淨的文件上載入
 * （HMR、同址導覽），照抄下來就會疊成 `(1) (2) Agents Manager`。
 */
const BASE_TITLE = (typeof document === 'undefined' ? 'Agents Manager' : document.title)
  .replace(/^\(\d+\+?\)\s*/, '')
  // 標題現在還帶著「畫面 · 」的前綴（`screenTitle`），同樣要剝掉才不會一路疊上去。
  .split(' · ')
  .slice(-1)[0]

/**
 * 抽屜蓋在主面板上，所以「點下去會換頁」的東西點完就要收起來。選取真的變了那一半由
 * `App` 裡的 render 期比較負責；這裡補的是選取**沒**變的那一半——開抽屜看一眼、再點回
 * 本來就選著的那顆 bot（最常見的一種點法），`selectBot` 什麼都沒改，抽屜就賴在原地。
 *
 * 用 capture 是因為列上的收合鍵與 `⋯` 選單會 `stopPropagation()`，冒泡階段收不到；
 * 它們與改名、＋ 都不是導覽，所以另外列一組排除。會在抽屜裡開對話框的按鈕（新增 Bot、
 * 刪除專案…）一樣要留著抽屜：抽屜一收，`inert` 就把那張對話框一起關進去了。
 */
const DRAWER_NAV = '.bot-row, .project-head, .team-node-btn'
const DRAWER_STAY =
  '.bot-kids-toggle, .bot-actions, .bot-name-btn, .bot-name-input, .project-fold, .project-head-actions, .team-node-chev'

/**
 * 未讀的兩件全域雜事：分頁標題的 `(N)`，以及「視窗回到前景 = 現在開著的那個對話被讀到了」。
 *
 * 前景這一段一定要有：使用者常常是把分頁切走、回來才看到回覆。沒有它的話，人在畫面前面
 * 看著訊息，徽章卻還亮著（進來的當下他不在，clear 的時機就永遠不會到）。
 */
function useUnread() {
  const total = useStore((s) => totalUnread(s.botUnread))
  // 每個畫面有自己的網址，標題就該跟著說出「這是哪一個畫面」——分頁列上分得出來，
  // 加到主畫面的捷徑也才有名字。
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

/**
 * ⌥↑ / ⌥↓ switches bot from anywhere, so you can move between bots without leaving the
 * composer. Plain ↑/↓ is left alone: it belongs to whatever has focus (the composer's own
 * text, the sidebar listbox, a select). Inside a bot row ⌥↑/↓ already means "reorder", and
 * a dialog owns the keyboard while it is open — both are skipped here.
 */
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
  // SPEC-team §11.5：`teamLaunch` / `selectedTeamId` 與 `selectedProjectId` 互斥。
  const teamLaunch = useStore((s) => s.teamLaunch)
  const teamId = useStore((s) => s.selectedTeamId)
  // 主機 shell：同樣與上面每一個互斥，而且排在最前面——它是使用者剛剛按出來的暫時性視圖。
  const shellView = useStore((s) => s.shellView)
  // 只為了下面那個 key：ChatPanel 自己從 store 讀 selectedBotId。
  const botId = useStore((s) => s.selectedBotId)
  // 設定面板開在主面板那一側（`ChatPanel` 裡貼著齒輪），所以它一開，抽屜也該讓開。
  const settingsBotId = useStore((s) => s.settingsBotId)
  const [drawer, setDrawer] = useState(false)
  // Below this width the sidebar is an off-canvas drawer (styles.css `@media (width <= 1024px)`);
  // above it, it is a plain column that is always on screen and must stay reachable.
  const isMobile = useMediaQuery(DRAWER_QUERY)
  useViewportPin()
  const sidebarRef = useRef<HTMLElement>(null)
  const drawerOpen = isMobile && drawer

  useEffect(() => {
    void bootstrap()
  }, [bootstrap])

  useBotSwitchKeys()
  useUnread()
  // 抽屜借一格歷史：開著時按上一頁是關抽屜，不是離開這個畫面。
  useDrawerRoute(drawerOpen, () => setDrawer(false))

  // Picking anything in the drawer navigates the main panel, which the drawer is covering —
  // close it so the result is visible. Rotating to landscape (or any resize past the
  // breakpoint) drops the flag too, so the drawer does not spring back open on the way in.
  // Compared during render rather than from an effect: that is React's own answer for
  // "adjust state when a prop changes", and it avoids the extra paint of the stale open
  // drawer that a post-render effect would leave on screen for a frame.
  const selection = `${botId ?? ''}|${groupProjectId ?? ''}|${teamId ?? ''}|${teamLaunch ? `${teamLaunch.projectId}:${teamLaunch.issueNumber}` : ''}|${shellView ? `${shellView.host}:${shellView.paneId}` : ''}|${settingsBotId ?? ''}`
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

  // Over the main panel with a scrim, the drawer is modal: keep Tab inside it and hand focus
  // back to the header button that opened it.
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
        // Only at the drawer breakpoint, and only while closed: there the sidebar is parked
        // offscreen, where Tab and a screen reader would otherwise still walk through it.
        // On desktop it is a visible column, so it must never be inert.
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
        {/* 選著 bot 時 shell 掛在 ChatPanel 的標題列底下（當第三個分頁）；其他選取
            （group / team / 沒選）才整個換成 shell 面板。 */}
        {shellView && (teamLaunch || teamId || groupProjectId || !botId) ? (
          <HostShellPanel
            key={`${shellView.host}:${shellView.paneId}`}
            host={shellView.host}
            paneId={shellView.paneId}
            cwd={shellView.cwd}
            onOpenSidebar={() => setDrawer(true)}
          />
        ) : teamLaunch ? (
          <TeamLaunchPanel
            key={`${teamLaunch.projectId}:${teamLaunch.repo}:${teamLaunch.issueNumber}`}
            projectId={teamLaunch.projectId}
            issueNumber={teamLaunch.issueNumber}
            repo={teamLaunch.repo}
            onOpenSidebar={() => setDrawer(true)}
          />
        ) : teamId ? (
          <TeamPanel key={teamId} teamId={teamId} onOpenSidebar={() => setDrawer(true)} />
        ) : groupProjectId ? (
          <GroupChatPanel key={groupProjectId} projectId={groupProjectId} onOpenSidebar={() => setDrawer(true)} />
        ) : (
          // 跟 TeamPanel / GroupChatPanel 一樣要 key：換 bot 就重新掛載，還沒送出的圖片
          // （useAttachments）、捲動位置等本地狀態才不會跟著跑到下一個 bot 身上。草稿存在
          // store 裡（依 bot 分開），不受重新掛載影響。
          <ChatPanel key={botId ?? 'none'} onOpenSidebar={() => setDrawer(true)} />
        )}
      </main>
      {/* 版面上的第三格（桌機在右緣、≤1024px 在底部），刻意掛在 `main` 外面：換 bot /
          project / team 都不會 unmount，暫存的圖片才跨得過去。 */}
      <ImageShelf />
      <Notices />
    </div>
  )
}
