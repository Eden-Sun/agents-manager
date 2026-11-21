import { useState } from 'react'
import { sendQueuedNow } from '../api'
import type { Message } from '../api/types'
import { ApiError } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'

export function SendQueuedNowButton({ botId, inFlightTurnId, inputEmpty }: { botId: string; inFlightTurnId: string | null | undefined; inputEmpty: boolean }) {
  const t = useT()
  const isClaude = useStore((s) => s.bots.find((b) => b.id === botId)?.kind === 'claude')
  const hasSupplement = useStore((s) =>
    Boolean(inFlightTurnId) && (s.messages[botId] ?? []).some((m) => m.turn_id === inFlightTurnId && m.role === 'user' && m.sent_via === 'supplement'),
  )
  const { busy, click } = useSendQueuedNow(botId)
  if (!inFlightTurnId || !isClaude || !hasSupplement || !inputEmpty) return null
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

export function SupplementSendNow({ msg: m }: { msg: Pick<Message, 'role' | 'sent_via' | 'turn_id' | 'bot_id'> }) {
  const t = useT()
  const botId = m.bot_id ?? ''
  const eligible = useStore((s) =>
    m.role === 'user' && m.sent_via === 'supplement' && Boolean(botId) && Boolean(m.turn_id) &&
    s.bots.find((b) => b.id === botId)?.kind === 'claude' &&
    Object.values(s.turns[botId] ?? {}).some((x) => x.id === m.turn_id && x.status === 'in_flight'),
  )
  const [gone, setGone] = useState(false)
  const { busy, click } = useSendQueuedNow(botId, () => setGone(true))
  if (!eligible || gone) return null
  return (
    <button
      type="button"
      className="queued-jump"
      disabled={busy}
      aria-label={t('插隊送出：打斷目前這一輪，立刻送出這則排隊中的補充')}
      title={t('補充還排在終端佇列（CLI 正在跑工具）時，打斷目前這一輪、立刻送進去')}
      onClick={() => void click()}
    >
      <span className="queued-jump-icon" aria-hidden="true">⤴</span>
      {t('插隊送出')}
    </button>
  )
}
