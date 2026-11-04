import { useEffect } from 'react'
import * as api from '../api'
import { t } from '../lib/i18n'
import { forgetLoginShell, isLoginShell, loginSucceeded } from '../lib/loginShellWatch'
import { useStore } from '../store/store'

const POLL_MS = 2_000
/** 提示後等多久才關：給人看一眼結果、或按「保留」。 */
export const CLOSE_DELAY_MS = 4_000

/** 為登入開的 shell（`lib/loginShellWatch`）：讀到登入成功就提示、幾秒後收掉 pane，並重新偵測登入狀態。 */
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
        // 讀不到（pane 已關、暫時斷線）：下一拍再試，pane 真的沒了面板自己會收。
      }
      if (alive) poll = setTimeout(() => void tick(), POLL_MS)
    }
    void tick()
    return () => {
      alive = false
      if (poll) clearTimeout(poll)
      // 計時器不清：使用者切走了，登入已成功，照樣收 pane。
    }
  }, [host, paneId])
}
