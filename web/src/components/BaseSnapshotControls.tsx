import { useCallback, useEffect, useState } from 'react'
import { clearBaseSnapshot, getBaseSnapshot, lockBaseSnapshot, type BaseSnapshot } from '../api/baseSnapshot'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'

/** codex bot 的基準快照：把目前的 context 鎖成基準，排程可選「每次從基準快照開始」（桌面、手機共用）。 */
export function BaseSnapshotControls({ botId, name }: { botId: string; name: string }) {
  const t = useT()
  const notify = useStore((s) => s.notify)
  const [snap, setSnap] = useState<BaseSnapshot | null>(null)
  const [busy, setBusy] = useState(false)
  const errText = (e: unknown) => {
    const raw = e instanceof Error ? e.message : String(e)
    if (raw.includes('base_snapshot_no_session')) return t('這顆 Bot 還沒有對話可鎖定，請先跟它對話')
    if (raw.includes('base_snapshot_not_codex')) return t('基準快照只支援 codex Bot')
    return raw
  }
  const load = useCallback(async () => {
    try {
      setSnap(await getBaseSnapshot(botId))
    } catch {
      setSnap(null)
    }
  }, [botId])
  useEffect(() => {
    setSnap(null)
    void load()
  }, [load])

  const run = async (act: () => Promise<BaseSnapshot>, ok: string) => {
    setBusy(true)
    try {
      setSnap(await act())
      notify('info', ok)
    } catch (e) {
      notify('error', errText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="field base-snapshot-controls">
      <span>{t('基準快照')}</span>
      <div className="bs-login-row">
        <button type="button" className="btn" disabled={busy} onClick={() => void run(() => lockBaseSnapshot(botId), t('已把 {name} 目前的 context 鎖成基準', { name }))}>
          {t('鎖定目前 context 為基準')}
        </button>
        <button type="button" className="btn" disabled={busy || !snap?.locked} onClick={() => void run(() => clearBaseSnapshot(botId), t('已清除 {name} 的基準', { name }))}>
          {t('清除基準')}
        </button>
      </div>
      <p className="hint base-snapshot-status">
        {snap?.locked ? t('已鎖定於 {time}', { time: snap.locked_at ? new Date(snap.locked_at).toLocaleString() : '' }) : t('尚未鎖定基準')}
      </p>
      <p className="hint">{t('排程可選「每次從基準快照開始」：每次觸發都從這個基準 fork 一份新對話執行。鎖定後請先別再跟這顆 Bot 手動對話，否則基準會跟著變長。')}</p>
    </div>
  )
}
