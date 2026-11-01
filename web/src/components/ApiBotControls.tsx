import { useState } from 'react'
import * as api from '../api'
import { retireApiSession } from '../api/apiBotSession'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'

/** claude-api bot 的停止與清除對話（沒有終端與重啟，這兩個是它僅有的生命週期操作；2026-11-01）。 */
export function ApiBotControls({ botId, name }: { botId: string; name: string }) {
  const t = useT()
  const notify = useStore((s) => s.notify)
  const refresh = useStore((s) => s.refreshState)
  const active = useStore((s) => {
    const st = s.runs[botId]?.state
    return st === 'running' || st === 'starting'
  })
  const [busy, setBusy] = useState<'stop' | 'retire' | null>(null)
  const [confirm, setConfirm] = useState(false)
  const errText = (e: unknown) => (e instanceof Error ? e.message : String(e))
  const retireErrText = (e: unknown) => {
    const raw = errText(e)
    if (raw.includes('api_retire_busy')) return t('bot 仍有進行中的回合或關閉尚未完成，請稍後再試')
    if (raw.includes('api_retire_unresolved')) return t('對話還有未結清的紀錄，暫時無法清除')
    if (raw.includes('api_retire_unconfirmed')) return t('無法確認清除結果，請重新整理後再確認')
    if (raw.includes('api_retire_storage')) return t('資料庫暫時無法寫入，請稍後再試')
    return raw
  }

  const stop = async () => {
    setBusy('stop')
    try {
      await api.stopBot(botId)
      notify('info', t('{name} 已停止', { name }))
    } catch (e) {
      notify('error', t('停止失敗：{reason}', { reason: errText(e) }))
    } finally {
      setBusy(null)
      void refresh()
    }
  }
  const retire = async () => {
    setConfirm(false)
    setBusy('retire')
    try {
      const done = await retireApiSession(botId)
      notify('info', done ? t('已清除 {name} 的對話，下一則從新對話開始', { name }) : t('{name} 目前沒有可清除的對話', { name }))
    } catch (e) {
      notify('error', t('清除對話失敗：{reason}', { reason: retireErrText(e) }))
    } finally {
      setBusy(null)
      void refresh()
    }
  }

  return (
    <div className="field api-bot-controls">
      <span>{t('Claude API 對話')}</span>
      <div className="bs-login-row">
        <button type="button" className="btn" disabled={!active || busy !== null} onClick={() => void stop()}>
          {busy === 'stop' ? t('停止中…') : t('停止')}
        </button>
        <button type="button" className="btn" disabled={busy !== null} onClick={() => setConfirm(true)}>
          {busy === 'retire' ? t('清除中…') : t('清除對話')}
        </button>
      </div>
      <p className="hint">{t('停止後再送訊息會接回同一段對話；清除對話會刪掉這段對話的上下文，下一則從頭開始（帳目保留）。')}</p>
      <ConfirmDialog
        open={confirm}
        title={t('清除對話')}
        confirmLabel={t('清除對話')}
        danger
        body={t('要清除 {name} 目前的對話嗎？它會忘記之前的內容，無法復原。', { name })}
        onCancel={() => setConfirm(false)}
        onConfirm={() => void retire()}
      />
    </div>
  )
}
