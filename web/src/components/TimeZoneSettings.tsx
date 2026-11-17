import { useEffect, useMemo, useState } from 'react'
import { setTimeZone } from '../api/timeZone'
import { ApiError } from '../api/types'
import { useT } from '../lib/i18n'
import { timeZoneOptions } from '../schedules/format'
import { utcOffsetLabel } from '../lib/timeZone'
import { useStore } from '../store/store'

/** 環境設定→顯示：網頁顯示時間用的時區（#2047）。bot 對話用的時區在新啟動的 bot 生效。 */
export function TimeZoneSettings() {
  const t = useT()
  const zone = useStore((s) => s.userTimeZone)
  const setUserTimeZone = useStore((s) => s.setUserTimeZone)
  const [draft, setDraft] = useState(zone)
  const [busy, setBusy] = useState(false)
  const [msg, setMsg] = useState<string | null>(null)
  useEffect(() => setDraft(zone), [zone])
  const zones = useMemo(() => timeZoneOptions(zone), [zone])

  const save = async () => {
    setBusy(true)
    setMsg(null)
    try {
      const r = await setTimeZone(draft.trim())
      setUserTimeZone(r.time_zone)
      setDraft(r.time_zone)
      setMsg(t('已儲存'))
    } catch (e) {
      setMsg(e instanceof ApiError && e.status === 400 ? t('時區名稱無效') : e instanceof Error ? e.message : t('操作失敗'))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="time-zone-settings">
      <div className="time-zone-settings-title">{t('時區')}</div>
      <p className="time-zone-settings-note">
        {t('目前 UTC{offset}', { offset: utcOffsetLabel(zone) })}
      </p>
      <div className="time-zone-settings-row">
        <input
          type="text"
          list="time-zone-options"
          aria-label={t('時區')}
          value={draft}
          disabled={busy}
          onChange={(e) => setDraft(e.target.value)}
        />
        <datalist id="time-zone-options">
          {zones.map((z) => (
            <option key={z} value={z} />
          ))}
        </datalist>
        <button type="button" className="btn" disabled={busy || !draft.trim()} onClick={() => void save()}>
          {t('儲存')}
        </button>
      </div>
      <p className="time-zone-settings-note" role="status">
        {msg ?? t('bot 對話用的時區在新啟動的 bot 生效')}
      </p>
    </div>
  )
}
