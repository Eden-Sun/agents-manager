import { useState } from 'react'
import { fetchMessages } from '../api'
import type { ScheduleAttempt, ScheduleOccurrence, ScheduleOutcome } from '../api/schedules'
import { t } from '../lib/i18n'
import './scheduleResult.css'

export function outcomeLabel(outcome: ScheduleOutcome): string {
  return outcome === 'success' ? t('任務成功') : outcome === 'failed' ? t('任務失敗') : outcome === 'refused' ? t('拒絕執行') : t('任務結果不明')
}
function Attempt({ attempt, botId, onConversation }: { attempt: ScheduleAttempt; botId: string; onConversation: () => void }) {
  const [reply, setReply] = useState<string | null>(null)
  const [error, setError] = useState(false)
  const [loading, setLoading] = useState(false)
  const load = async () => {
    if (!attempt.turn_id || reply !== null || loading) return
    setLoading(true)
    setError(false)
    try {
      const page = await fetchMessages(botId, 200, undefined, { turnId: attempt.turn_id, role: 'assistant' })
      const msg = page.messages.find((m) => m.id === attempt.reply_message_id)
      if (!msg) setError(true)
      else setReply(msg.content)
    } catch { setError(true) }
    finally { setLoading(false) }
  }
  return <div className="schedule-result">
    <span className={`schedule-outcome ${attempt.outcome}`}>{outcomeLabel(attempt.outcome)}</span> {t('第 {n} 次', { n: attempt.attempt_no })}
    <p>{attempt.outcome_summary}</p>
    {attempt.reply_message_id && attempt.turn_id ? <details onToggle={(e) => { if (e.currentTarget.open) void load() }}>
      <summary>{t('查看該回合完整回覆')}</summary>
      {loading ? <p>{t('載入中…')}</p> : null}
      {error ? <p role="alert">{t('回覆無法讀取，請開啟 Bot 對話。')}</p> : null}
      {reply !== null ? <pre>{reply}</pre> : null}
      <button type="button" className="btn" onClick={onConversation}>{t('開啟 Bot 對話')}</button>
    </details> : null}
  </div>
}
export function ScheduleResult({ occurrence: o, onConversation }: { occurrence: ScheduleOccurrence; onConversation: () => void }) {
  const attempts: ScheduleAttempt[] = [...(o.attempts ?? [])]
  const outcome = o.status === 'cancelled' ? null : o.outcome ?? (['done', 'failed', 'unknown'].includes(o.status) ? 'unknown' : null)
  if (outcome && !attempts.some((a) => a.attempt_no === (o.attempt_no ?? 1)) && o.status !== 'pending' && o.status !== 'queued') attempts.push( { attempt_no: o.attempt_no ?? 1, turn_id: o.turn_id, reply_message_id: o.reply_message_id ?? null, outcome, outcome_summary: o.outcome_summary ?? '' })
  return <>
    {o.status === 'pending' && o.next_retry_at ? <p>{t('等待重試（第 {n} 次執行）', { n: o.attempt_no ?? 1 })}</p> : null}
    {attempts.length === 0 ? <p>{o.status === 'cancelled' ? t('已取消，未產生任務結果') : t('等待任務結果')}</p> : null}
    {attempts.map((a) => <Attempt key={a.attempt_no} attempt={a} botId={o.bot_id} onConversation={onConversation} />)}
  </>
}
