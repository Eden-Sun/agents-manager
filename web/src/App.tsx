import { useEffect, useState } from 'react'
import { MOCK_MODE } from './api'
import { ChatPanel } from './components/ChatPanel'
import { Sidebar } from './components/Sidebar'
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

export default function App() {
  const ready = useStore((s) => s.ready)
  const bootError = useStore((s) => s.bootError)
  const bootstrap = useStore((s) => s.bootstrap)
  const [drawer, setDrawer] = useState(false)

  useEffect(() => {
    void bootstrap()
  }, [bootstrap])

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
        <ChatPanel onOpenSidebar={() => setDrawer(true)} />
      </main>
      <Notices />
    </div>
  )
}
