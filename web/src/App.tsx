import { useEffect, useState } from 'react'
import { MOCK_MODE } from './api'
import { ChatPanel } from './components/ChatPanel'
import { GroupChatPanel } from './components/GroupChatPanel'
import { Sidebar } from './components/Sidebar'
import { TeamLaunchPanel } from './components/TeamLaunchPanel'
import { TeamPanel } from './components/TeamPanel'
import { useStore } from './store/store'

function Notices() {
  const notices = useStore((s) => s.notices)
  const dismiss = useStore((s) => s.dismiss)
  if (notices.length === 0) return null
  return (
    <div className="notices" role="status" aria-live="polite">
      {notices.map((n) => (
        <div key={n.id} className={`notice ${n.kind}`}>
          <span style={{ flex: 1 }}>{n.text}</span>
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
  const refreshState = useStore((s) => s.refreshState)
  // socket 通、herdr 斷：daemon 之後會推狀態，但別乾等——每 3 秒（和回到分頁時）自己抓一次。
  const herdrDown = socket === 'open' && !connected
  useEffect(() => {
    if (!herdrDown) return
    const tick = () => void refreshState()
    const id = setInterval(tick, 3_000)
    window.addEventListener('focus', tick)
    return () => {
      clearInterval(id)
      window.removeEventListener('focus', tick)
    }
  }, [herdrDown, refreshState])
  if (socket === 'open' && connected) return null
  const label =
    socket !== 'open'
      ? socket === 'connecting'
        ? '正在重新連線 daemon…'
        : '與 daemon 的連線中斷，正在重試…'
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
  // 只為了下面那個 key：ChatPanel 自己從 store 讀 selectedBotId。
  const botId = useStore((s) => s.selectedBotId)
  const [drawer, setDrawer] = useState(false)

  useEffect(() => {
    void bootstrap()
  }, [bootstrap])

  useBotSwitchKeys()

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
      <aside className={`sidebar${drawer ? ' open' : ''}`}>
        <Sidebar />
      </aside>
      {drawer ? <button type="button" className="scrim" aria-label="關閉側邊欄" onClick={() => setDrawer(false)} /> : null}
      <main className="main">
        <ConnBanner />
        {teamLaunch ? (
          <TeamLaunchPanel
            key={`${teamLaunch.projectId}:${teamLaunch.issueNumber}`}
            projectId={teamLaunch.projectId}
            issueNumber={teamLaunch.issueNumber}
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
      <Notices />
    </div>
  )
}
