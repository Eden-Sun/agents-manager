import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import App from './App.tsx'
import './styles.css'
import { startRouteSync } from './store/routeSync'
import { startShelfPersistence } from './store/shelfPersist'

startShelfPersistence()
// 網址 ↔ store 的雙向同步。掛在 render 之前，開頁的路徑才不會被第一次繪製吃掉。
startRouteSync()

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
