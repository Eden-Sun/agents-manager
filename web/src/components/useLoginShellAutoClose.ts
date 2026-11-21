import { useEffect } from 'react'
import * as api from '../api'
import { t } from '../lib/i18n'
import { forgetLoginShell, isLoginShell, loginSucceeded } from '../lib/loginShellWatch'
import { useStore } from '../store/store'

const POLL_MS = 2_000
export const CLOSE_DELAY_MS = 4_000

export function useLoginShellAutoClose(host: string, paneId: string) {
  useEffect(() => {
    if (!isLoginShell(host, paneId)) return
    let alive = true
    let poll: ReturnType<typeof setTimeout> | null = null
    let close: ReturnType<typeof setTimeout> | null = null
    const tick = async () => {
      if (!alive) return
      try {
        const snap = await api.readHostShell(host, paneId, 'visible', 200)
        if (alive && loginSucceeded(snap.text)) {
          forgetLoginShell(host, paneId)
          const s = useStore.getState()
          void s.refreshTools(host)
          s.notify('info', t('登入完成，{s} 秒後自動關閉這個 shell', { s: CLOSE_DELAY_MS / 1000 }), {
            label: t('保留'),
            run: () => {
              if (close) clearTimeout(close)
              close = null
            },
          })
          close = setTimeout(() => {
            close = null
            void useStore.getState().endHostShell(host, paneId)
          }, CLOSE_DELAY_MS)
          return
        }
      } catch {
      }
      if (alive) poll = setTimeout(() => void tick(), POLL_MS)
    }
    void tick()
    return () => {
      alive = false
      if (poll) clearTimeout(poll)
    }
  }, [host, paneId])
}
