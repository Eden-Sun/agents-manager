import { useId, type CSSProperties } from 'react'
import type { RuntimeSettings } from '../api/types'
import { useT } from '../lib/i18n'
import { CONTEXT_TICKS, HOT_COMPACT_MAX, HOT_COMPACT_MIN, HOT_PCT_MAX, KEEP_WARM_MAX, KEEP_WARM_MIN, formatContext, nearestTickIndex, numberOrNull } from '../lib/runtimePolicy'
import './runtimeSettings.css'

/**
 * 執行期設定的欄位（角色卡與 bot 覆寫共用）。`null`/缺＝「跟上一層」（角色卡：跟預設；bot：跟角色）。
 * `inheritLabel` 是「跟…」那顆的字（角色卡＝跟預設、bot 覆寫＝跟角色）。
 */
export function RuntimeSettingsFields({
  value,
  onChange,
  inheritLabel,
  idPrefix,
  disabled,
  codexNote,
}: {
  value: RuntimeSettings
  onChange: (next: RuntimeSettings) => void
  inheritLabel: string
  idPrefix: string
  disabled?: boolean
  /** 只有 codex bot 的覆寫框才顯示「codex 永快取」提示（角色卡上講一次就好，見 RolePolicyEditor 的說明）。 */
  codexNote?: boolean
}) {
  const t = useT()
  const uid = useId()
  const ctxId = `${idPrefix}-${uid}-ctx`
  const set = (patch: RuntimeSettings) => onChange({ ...value, ...patch })
  const ctxIdx = nearestTickIndex(value.context_tokens)
  const pill = (on: boolean) => `opt${on ? ' on' : ''}`

  return (
    <div className="rt-fields">
      <div className="field">
        <div className="rt-ctx-head">
          <span id={`${ctxId}-label`}>{t('context 上限')}</span>
          <span className="rt-ctx-value">{value.context_tokens == null ? inheritLabel : formatContext(value.context_tokens)}</span>
          {value.context_tokens != null ? (
            <button type="button" className="rt-ctx-reset" disabled={disabled} onClick={() => set({ context_tokens: null })}>
              {inheritLabel}
            </button>
          ) : null}
        </div>
        {/* 刻度與拉條同寬、逐格對齊（位置扣掉拇指半寬）；沒設時拇指淡化，不假裝停在 200k。 */}
        <div className={`rt-ctx-slider${value.context_tokens == null ? ' inherit' : ''}`} style={{ '--rt-steps': CONTEXT_TICKS.length - 1 } as CSSProperties}>
          <input
            type="range"
            aria-labelledby={`${ctxId}-label`}
            aria-valuetext={value.context_tokens == null ? inheritLabel : formatContext(value.context_tokens)}
            min={0}
            max={CONTEXT_TICKS.length - 1}
            step={1}
            disabled={disabled}
            value={ctxIdx}
            onChange={(e) => set({ context_tokens: CONTEXT_TICKS[Number(e.target.value)] })}
          />
          <div className="rt-ticks" aria-hidden="true">
            {CONTEXT_TICKS.map((n, i) => (
              <span key={n} className={value.context_tokens === n ? 'on' : undefined} style={{ '--rt-i': i } as CSSProperties}>
                {formatContext(n)}
              </span>
            ))}
          </div>
        </div>
      </div>

      <div className="field">
        <span id={`${ctxId}-keep`}>{t('保溫')}</span>
        <div className="opt-group" role="radiogroup" aria-labelledby={`${ctxId}-keep`}>
          <button type="button" role="radio" aria-checked={value.keep_warm == null} className={pill(value.keep_warm == null)} disabled={disabled} onClick={() => set({ keep_warm: null })}>
            {inheritLabel}
          </button>
          <button type="button" role="radio" aria-checked={value.keep_warm === true} className={pill(value.keep_warm === true)} disabled={disabled} onClick={() => set({ keep_warm: true })}>
            {t('開')}
          </button>
          <button type="button" role="radio" aria-checked={value.keep_warm === false} className={pill(value.keep_warm === false)} disabled={disabled} onClick={() => set({ keep_warm: false })}>
            {t('關')}
          </button>
        </div>
        {codexNote ? <span className="hint">{t('codex 永快取，不需保溫（這個設定只影響 claude）')}</span> : null}
      </div>

      {value.keep_warm === false ? null : <label className="field">
        <span>{t('保溫分鐘')}</span>
        <input
          type="number"
          inputMode="numeric"
          min={KEEP_WARM_MIN}
          max={KEEP_WARM_MAX}
          placeholder="58"
          disabled={disabled}
          value={value.keep_warm_after_min ?? ''}
          onChange={(e) => set({ keep_warm_after_min: numberOrNull(e.target.value) })}
        />
      </label>}

      <details className="rt-advanced">
        <summary>{t('進階')}</summary>
        <label className="field">
          <span>{t('熱壓分鐘')}</span>
          <input
            type="number"
            inputMode="numeric"
            min={HOT_COMPACT_MIN}
            max={HOT_COMPACT_MAX}
            placeholder="110"
            disabled={disabled}
            value={value.hot_compact_after_min ?? ''}
            onChange={(e) => set({ hot_compact_after_min: numberOrNull(e.target.value) })}
          />
        </label>
        <label className="field">
          <span>{t('熱壓門檻（context %）')}</span>
          <input
            type="number"
            inputMode="numeric"
            min={0}
            max={HOT_PCT_MAX}
            placeholder="30"
            disabled={disabled}
            value={value.hot_compact_min_pct ?? ''}
            onChange={(e) => set({ hot_compact_min_pct: numberOrNull(e.target.value) })}
          />
        </label>
      </details>
    </div>
  )
}
