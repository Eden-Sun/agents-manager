import { MOCK_MODE } from './api'
import { langFromStorageEvent, missingKeys, getLang } from './lib/i18n'
import { applyTheme, loadTheme, themeFromStorageEvent } from './lib/theme'
import { startRouteSync } from './store/routeSync'
import { startShelfPersistence } from './store/shelfPersist'
import { startResetCreditExpiryNotice, startResetCreditsNotice } from './store/resetCreditsNotice'

let stopRouteSync: (() => void) | undefined
let stopShelf: (() => void) | undefined
let stopCredits: (() => void) | undefined
let stopExpiry: (() => void) | undefined
let onStorage: ((e: StorageEvent) => void) | undefined

export function startAppServices(): void {
  stopShelf = startShelfPersistence()
  // 網址 ↔ store 的雙向同步。掛在 render 之前，開頁的路徑才不會被第一次繪製吃掉。
  stopRouteSync = startRouteSync()
  stopCredits = startResetCreditsNotice()
  stopExpiry = startResetCreditExpiryNotice()

  applyTheme(loadTheme())
  onStorage = (e: StorageEvent) => {
    themeFromStorageEvent(e.key, e.newValue)
    langFromStorageEvent(e.key, e.newValue)
  }
  window.addEventListener('storage', onStorage)

  // 介面語言：偏好存在這台瀏覽器（lib/i18n/lang.ts）；mock 另可用 `?lang=en` 固定語言（api/index.ts 建立 mock 前套用）。
  if (MOCK_MODE || import.meta.env.DEV) {
    // 除錯：切到英文走過畫面後，`__amI18n.missing()` 列出還沒有英文的 key。
    ;(window as unknown as { __amI18n: unknown }).__amI18n = { lang: getLang, missing: missingKeys }
  }
}

// 在 render 之前執行一次
startAppServices()

if (import.meta.hot) {
  import.meta.hot.accept()
  import.meta.hot.dispose(() => {
    if (onStorage) {
      window.removeEventListener('storage', onStorage)
      onStorage = undefined
    }
    stopRouteSync?.()
    stopShelf?.()
    stopCredits?.()
    stopExpiry?.()
  })
}
