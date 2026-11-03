import { useState } from 'react'
import { sendQueuedNow } from '../api'
import { ApiError } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'

/**
 * 「補充」被 claude CLI 排進終端佇列（正在跑工具）時，一鍵插隊送出（`POST /bots/:id/send-queued-now`）。
 * 只在：回合進行中、claude 支援 send-now、這一回合有補充、輸入框是空的（有字時「插隊」鈕已涵蓋）。
 * daemon 會讀畫面確認真的有排隊才按鍵；沒有就 409 `nothing_queued`。
 */
export function SendQueuedNowButton({ botId, inFlightTurnId, inputEmpty }: { botId: string; inFlightTurnId: string | null | undefined; inputEmpty: boolean }) {
  const t = useT()
  const isClaude = useStore((s) => s.bots.find((b) => b.id === botId)?.kind === 'claude')
  const sendNowSupported = useStore((s) => s.runs[botId]?.send_now_supported === true)
  const runId = useStore((s) => s.runs[botId]?.id ?? null)
  const hasSupplement = useStore((s) =>
    Boolean(inFlightTurnId) && (s.messages[botId] ?? []).some((m) => m.turn_id === inFlightTurnId && m.role === 'user' && m.sent_via === 'supplement'),
  )
  const notify = useStore((s) => s.notify)
  const [busy, setBusy] = useState(false)
  if (!inFlightTurnId || !isClaude || !sendNowSupported || !hasSupplement || !inputEmpty) return null
  const click = async () => {
    setBusy(true)
    try {
      await sendQueuedNow(botId, runId)
    } catch (e) {
      if (e instanceof ApiError && e.status === 409 && (e.body as { reason?: string }).reason === 'nothing_queued') notify('info', t('沒有排隊中的補充：已經送進去了'))
      else notify('error', e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }
  return (
    <button type="button" className="mini-btn" disabled={busy} title={t('補充還排在終端佇列（CLI 正在跑工具）時，打斷目前這一輪、立刻送進去')} onClick={() => void click()}>
      {t('送出排隊的補充')}
    </button>
  )
}
