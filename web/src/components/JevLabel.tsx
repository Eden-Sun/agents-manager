/**
 * 「Jev 建議」統一標示（issue #1912）：凡是 Jev（TypeSafe 型別判斷）給的判斷／建議，呈現給人的地方都用這個元件，
 * 讓人一眼看出「這是 Jev 的建議」＋機率，並說明只是建議、不會自動動作。
 *
 * 用法：
 *   <JevLabel prob={0.82} />             → 「Jev 建議 82%」膠囊（hover 有完整說明）
 *   <JevLabel prob={0.82} note />        → 膠囊後面再接「只是建議，不會自動動作」
 *   <JevLabel />                          → 沒有機率（例如撞限第二意見沒有門檻）就只有「Jev 建議」
 * `prob` 是 0–1；超出範圍或不是有限數字就當沒有機率。
 */
import { useT } from '../lib/i18n'
import { JEV_ADVISORY_NOTE, JEV_FULL_NAME, jevProbText } from '../lib/jevLabel'
import './jevLabel.css'

export function JevLabel({ prob, note = false, className }: { prob?: number | null; note?: boolean; className?: string }) {
  const t = useT()
  const p = jevProbText(prob)
  const name = t.tr(JEV_FULL_NAME)
  const advisory = t.tr(JEV_ADVISORY_NOTE)
  return (
    <span className={className ? `jev-label ${className}` : 'jev-label'} data-testid="jev-label">
      <span
        className="jev-label-tag"
        title={p ? t('{name}的建議，機率 {p}；{note}', { name, p, note: advisory }) : t('{name}的建議；{note}', { name, note: advisory })}
      >
        {p ? t('Jev 建議 {p}', { p }) : t('Jev 建議')}
      </span>
      {note ? <span className="jev-label-note">{advisory}</span> : null}
    </span>
  )
}
