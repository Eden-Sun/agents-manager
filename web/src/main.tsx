import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import App from './App.tsx'
import './styles.css'
import './components/mobileBotHead.css'
import './components/desktopBotHead.css'
import { MOCK_MODE } from './api'
import { langFromStorageEvent, missingKeys, getLang } from './lib/i18n'
import { applyTheme, loadTheme, themeFromStorageEvent } from './lib/theme'
import { startRouteSync } from './store/routeSync'
import { startShelfPersistence } from './store/shelfPersist'

startShelfPersistence()
// 網址 ↔ store 的雙向同步。掛在 render 之前，開頁的路徑才不會被第一次繪製吃掉。
startRouteSync()

applyTheme(loadTheme())
window.addEventListener('storage', (e) => {
  themeFromStorageEvent(e.key, e.newValue)
  langFromStorageEvent(e.key, e.newValue)
})
// 介面語言：偏好存在這台瀏覽器（lib/i18n/lang.ts）；mock 另可用 `?lang=en` 固定語言（api/index.ts 建立 mock 前套用）。
if (MOCK_MODE || import.meta.env.DEV) {
  // 除錯：切到英文走過畫面後，`__amI18n.missing()` 列出還沒有英文的 key。
  ;(window as unknown as { __amI18n: unknown }).__amI18n = { lang: getLang, missing: missingKeys }
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
