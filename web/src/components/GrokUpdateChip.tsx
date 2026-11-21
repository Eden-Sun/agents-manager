import { useState } from 'react'
import { useGrokPlan } from '../hooks/useGrokPlan'
import { grokChipLabel } from '../lib/grokUpdate'
import { changelogRange } from '../lib/upstreamRange'
import { useReviewAsk } from '../hooks/useReviewAsk'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpstreamChangelogBox } from './UpstreamChangelogBox'
import type { DialogControl } from './MergedUpdateChip'

export function GrokUpdateChip({ control }: { control?: DialogControl } = {}) {
  const t = useT()
  const plan = useGrokPlan()
  const [localOpen, setLocalOpen] = useState(false)
  const open = control ? control.open : localOpen
  const setOpen = (v: boolean) => (control && !v ? control.close() : setLocalOpen(v))
  const range = changelogRange(plan?.hosts ?? [])
  const review = useReviewAsk('grok', range.host, range.from, plan?.target ?? null)
  if (!plan) return null
  const label = grokChipLabel(plan)
  return (
    <>
      {control ? null : (
        <button type="button" className="quota-update waiting" title={label} aria-label={label} onClick={() => setOpen(true)}>
          <span aria-hidden="true"><UpgradeIcon /></span>
          <span className="quota-update-n herdr-chip-tag" aria-hidden="true">grok</span>
        </button>
      )}
      <ConfirmDialog
        open={open}
        title={t('grok 有新版 {version}', { version: plan.target })}
        confirmLabel={t('知道了')}
        cancelLabel={t('關閉')}
        {...review.dialog}
        body={
          <>
            {open ? <UpstreamChangelogBox kind="grok" host={range.host} from={range.from} to={plan.target} refreshKey={review.reviewKey} /> : null}
            <p>{t('這些主機的 grok 還不是最新的正式版：')}</p>
            <ul className="confirm-choices">
              {plan.hosts.map((h) => (
                <li key={h.host}>
                  {}
                  <strong>{h.host}</strong>：{h.from ?? t('讀不到版本')} → {plan.target}
                </li>
              ))}
            </ul>
            <p className="confirm-note">
              {t.x('到那台主機執行 {cmd}（grok 官方的升級指令）。這裡不代為安裝；裝好之後跑著的 grok bot 要重啟才會換成新版。', {
                cmd: <code>{plan.command}</code>,
              })}
            </p>
          </>
        }
        onConfirm={() => setOpen(false)}
        onCancel={() => setOpen(false)}
      />
    </>
  )
}
