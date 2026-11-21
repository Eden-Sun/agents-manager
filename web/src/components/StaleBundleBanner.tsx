import { useEffect, useState } from 'react'
import { useT } from '../lib/i18n'
import { checkServedBundle, dismissStaleBundle, loadedMainScript } from '../lib/staleBundle'
import { useStore } from '../store/store'
import './staleBundleBanner.css'

/** 頁面載入時的主 script（之後 DOM 不會變）；dev 模式是 null，整個偵測就不動作。 */
const LOADED = typeof document === 'undefined' ? null : loadedMainScript(document)

/**
 * 換版偵測：WS 連上（含重連）與分頁從背景回到前景時查一次。發現新前端就固定在頂部提示重新開啟；可收起，收起後同一版不再跳。
 */
export function StaleBundleBanner({ loaded = LOADED }: { loaded?: string | null } = {}) {
  const t = useT()
  const socket = useStore((s) => s.socket)
  const [stale, setStale] = useState<string | null>(null)

  useEffect(() => {
    if (socket !== 'open') return
    let alive = true
    void checkServedBundle(loaded).then((s) => {
      if (alive && s) setStale(s)
    })
    return () => {
      alive = false
    }
  }, [socket, loaded])

  useEffect(() => {
    const onVisible = () => {
      if (document.visibilityState !== 'visible') return
      void checkServedBundle(loaded).then((s) => {
        if (s) setStale(s)
      })
    }
    document.addEventListener('visibilitychange', onVisible)
    return () => document.removeEventListener('visibilitychange', onVisible)
  }, [loaded])

  if (!stale) return null
  return (
    <div className="stale-bundle-banner" role="alert">
      <span className="stale-bundle-text">{t('AG Man 已更新，請關閉後重新開啟')}</span>
      <button type="button" className="stale-bundle-reload" onClick={() => window.location.reload()}>
        {t('重新載入')}
      </button>
      <button
        type="button"
        className="stale-bundle-dismiss"
        aria-label={t('收起')}
        onClick={() => {
          dismissStaleBundle(stale)
          setStale(null)
        }}
      >
        ×
      </button>
    </div>
  )
}
