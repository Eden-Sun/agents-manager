import { MOCK_MODE } from './api'
import { langFromStorageEvent, missingKeys, getLang } from './lib/i18n'
import { applyTheme, loadTheme, themeFromStorageEvent } from './lib/theme'
import { startRouteSync } from './store/routeSync'
import { startShelfPersistence } from './store/shelfPersist'
import { startAttachmentDraftPersistence } from './store/composerAttachmentDrafts'
import { startResetCreditExpiryNotice, startResetCreditsNotice } from './store/resetCreditsNotice'
import { startResetCreditOfferNotice } from './store/resetCreditOffer'

let stopRouteSync: (() => void) | undefined
let stopShelf: (() => void) | undefined
let stopAttachmentDrafts: (() => void) | undefined
let stopCredits: (() => void) | undefined
let stopExpiry: (() => void) | undefined
let stopOffer: (() => void) | undefined
let onStorage: ((e: StorageEvent) => void) | undefined

export function startAppServices(): void {
  stopShelf = startShelfPersistence()
  stopAttachmentDrafts = startAttachmentDraftPersistence()
  stopRouteSync = startRouteSync()
  stopCredits = startResetCreditsNotice()
  stopExpiry = startResetCreditExpiryNotice()
  stopOffer = startResetCreditOfferNotice()

  applyTheme(loadTheme())
  onStorage = (e: StorageEvent) => {
    themeFromStorageEvent(e.key, e.newValue)
    langFromStorageEvent(e.key, e.newValue)
  }
  window.addEventListener('storage', onStorage)

  if (MOCK_MODE || import.meta.env.DEV) {
    ;(window as unknown as { __amI18n: unknown }).__amI18n = { lang: getLang, missing: missingKeys }
  }
}

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
    stopAttachmentDrafts?.()
    stopCredits?.()
    stopExpiry?.()
    stopOffer?.()
  })
}
