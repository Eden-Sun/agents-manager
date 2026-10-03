import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { httpShareClient, type ShareClient } from './shareApi'
import { ShareApp } from './ShareApp'
import { tokenFromLocation } from './shareModel'
import './share.css'

/**
 * 分享頁的入口（`web/share.html` → `dist/share.html`，daemon 的分享入口在 `/s/<token>` 回這頁）。
 * 刻意不 import 主 UI 的 store／api／元件：拿到連結的人＝網路上任何人，這一包裡不能有管理介面的程式碼。
 */
const MOCK = import.meta.env.VITE_MOCK === '1' || import.meta.env.VITE_MOCK === 'true'

async function boot() {
  const root = createRoot(document.getElementById('root')!)
  const token = tokenFromLocation(window.location)
  let client: ShareClient | null = null
  if (token) client = MOCK ? (await import('./shareMock')).mockShareClient(token) : httpShareClient(token)
  root.render(
    <StrictMode>
      {client ? (
        <ShareApp client={client} />
      ) : (
        <main className="sh-gone">
          <div className="sh-gone-card">
            <h1>連結不完整</h1>
            <p>請確認你打開的是完整的分享連結。</p>
          </div>
        </main>
      )}
    </StrictMode>,
  )
}

void boot()
