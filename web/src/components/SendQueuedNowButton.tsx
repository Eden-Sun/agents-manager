import { useState } from 'react'
import { sendQueuedNow } from '../api'
import type { Message } from '../api/types'
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
  const hasSupplement = useStore((s) =>
    Boolean(inFlightTurnId) && (s.messages[botId] ?? []).some((m) => m.turn_id === inFlightTurnId && m.role === 'user' && m.sent_via === 'supplement'),
  )
  const { busy, click } = useSendQueuedNow(botId)
  if (!inFlightTurnId || !isClaude || !sendNowSupported || !hasSupplement || !inputEmpty) return null
  return (
    <button type="button" className="mini-btn" disabled={busy} title={t('補充還排在終端佇列（CLI 正在跑工具）時，打斷目前這一輪、立刻送進去')} onClick={() => void click()}>
      {t('送出排隊的補充')}
    </button>
  )
}

function useSendQueuedNow(botId: string, onSettled?: () => void) {
  const t = useT()
  const runId = useStore((s) => s.runs[botId]?.id ?? null)
  const notify = useStore((s) => s.notify)
  const [busy, setBusy] = useState(false)
  const click = async () => {
    setBusy(true)
    try {
      await sendQueuedNow(botId, runId)
    } catch (e) {
      if (e instanceof ApiError && e.status === 409 && (e.body as { reason?: string }).reason === 'nothing_queued') {
        notify('info', t('沒有排隊中的補充：已經送進去了'))
        onSettled?.()
      } else notify('error', e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }
  return { busy, click }
}

/** 對話裡標「補充」的那則旁的小鈕：該則屬於進行中的回合、claude 支援 send-now 時才畫；回 nothing_queued 就藏起來。 */
export function SupplementSendNow({ msg: m }: { msg: Pick<Message, 'role' | 'sent_via' | 'turn_id' | 'bot_id'> }) {
  const t = useT()
  const botId = m.bot_id ?? ''
  const eligible = useStore((s) =>
    m.role === 'user' && m.sent_via === 'supplement' && Boolean(botId) && Boolean(m.turn_id) &&
    s.bots.find((b) => b.id === botId)?.kind === 'claude' && s.runs[botId]?.send_now_supported === true &&
    Object.values(s.turns[botId] ?? {}).some((x) => x.id === m.turn_id && x.status === 'in_flight'),
  )
  const [gone, setGone] = useState(false)
  const { busy, click } = useSendQueuedNow(botId, () => setGone(true))
  if (!eligible || gone) return null
  return (
    <button type="button" className="mini-btn" disabled={busy} title={t('補充還排在終端佇列（CLI 正在跑工具）時，打斷目前這一輪、立刻送進去')} onClick={() => void click()}>
      {t('插隊')}
    </button>
  )
}
