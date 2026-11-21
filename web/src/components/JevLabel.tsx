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
