import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { t } from './shareI18n'
import { httpShareClient, type ShareClient } from './shareApi'
import { ShareApp } from './ShareApp'
import { tokenFromLocation } from './shareModel'
import './share.css'

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
            <h1>{t('連結不完整')}</h1>
            <p>{t('請確認你打開的是完整的分享連結。')}</p>
          </div>
        </main>
      )}
    </StrictMode>,
  )
}

void boot()
