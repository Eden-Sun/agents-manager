import { t } from '../lib/i18n'

/** 一天內的一個時段。`until === null` 是固定時間。 */
export interface WhenSlot {
  time: string
  until: string | null
}

/**
 * 「何時執行」的時段列：一行 `開始 ~ 結束`、隨機／固定圓鈕、刪除。
 * 不碰 spec；呼叫端沿用原本的 time／until 與驗證。
 */
export function ScheduleWhenSlots({
  slots,
  tz,
  modeLabel,
  loading,
  maxSlots,
  errorOf,
  onChange,
  onToggle,
  onRemove,
  onAdd,
}: {
  slots: WhenSlot[]
  tz: string
  modeLabel: string
  loading: boolean
  maxSlots: number
  errorOf: (slot: WhenSlot) => string | null
  onChange: (index: number, patch: Partial<WhenSlot>) => void
  onToggle: (index: number) => void
  onRemove: (index: number) => void
  onAdd: () => void
}) {
  const summary = slots
    .map((slot) => {
      if (errorOf(slot)) return ''
      return slot.until != null
        ? t('{range} 隨機', { range: `${slot.time}–${slot.until}` })
        : t('{time} 固定', { time: slot.time })
    })
    .filter(Boolean)
    .join('、')

  return (
    <div className="schedule-slots">
      {slots.map((slot, i) => {
        const random = slot.until != null
        const error = errorOf(slot)
        return (
          <div key={i} className={random ? 'schedule-slot-row is-random' : 'schedule-slot-row'}>
            <div className="schedule-slot-times">
              <input
                type="time"
                aria-label={t('第 {n} 個時段的時間', { n: i + 1 })}
                value={slot.time}
                disabled={loading}
                onChange={(e) => onChange(i, { time: e.target.value })}
              />
              {random ? (
                <>
                  <span className="schedule-slot-tilde" aria-hidden="true">~</span>
                  <input
                    type="time"
                    className="schedule-slot-until"
                    aria-label={t('第 {n} 個時段的結束時間', { n: i + 1 })}
                    aria-invalid={error ? true : undefined}
                    value={slot.until ?? ''}
                    disabled={loading}
                    onChange={(e) => onChange(i, { until: e.target.value })}
                  />
                </>
              ) : null}
            </div>
            <button
              type="button"
              className={random ? 'schedule-slot-mode is-random' : 'schedule-slot-mode'}
              aria-pressed={random}
              aria-label={random ? t('第 {n} 個時段：隨機區間', { n: i + 1 }) : t('第 {n} 個時段：固定時間', { n: i + 1 })}
              disabled={loading}
              onClick={() => onToggle(i)}
            >
              {random ? t('隨機') : t('固定')}
            </button>
            <button
              type="button"
              className="schedule-slot-delete"
              aria-label={t('刪除時段')}
              disabled={loading || slots.length <= 1}
              onClick={() => onRemove(i)}
            >
              <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
                <path d="M3 4.5h10M6.2 4.4V3.2h3.6v1.2M4.4 4.5l.6 8.2h6l.6-8.2" />
              </svg>
              {t('刪除')}
            </button>
            {error ? <p className="hint schedule-error schedule-slot-error" role="alert">{error}</p> : null}
          </div>
        )
      })}
      <div className="schedule-when-preview" aria-live="polite">
        <span className="schedule-when-preview-label">{t('執行預覽')}</span>
        <p className="schedule-when-meta">{modeLabel} · {tz}</p>
        {summary ? <p className="schedule-when-sum">{summary}</p> : null}
      </div>
      <button type="button" className="schedule-add-slot" disabled={loading || slots.length >= maxSlots} onClick={onAdd}>
        {t('＋ 新增時段')}
      </button>
    </div>
  )
}
