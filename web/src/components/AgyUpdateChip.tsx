import { useState } from 'react'
import { useUpdateChipRequest } from '../hooks/useUpdateChipRequest'
import { useStore } from '../store/store'
import { useAgyBusy, useAgyPlan } from '../hooks/useAgyPlan'
import { agyChipLabel, agyUpdateTitle } from '../lib/agyUpdate'
import { changelogRange } from '../lib/upstreamRange'
import { useReviewAsk } from '../hooks/useReviewAsk'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { AgyUpdateBody } from './AgyUpdateConfirm'
import { UpstreamChangelogBox } from './UpstreamChangelogBox'
import type { DialogControl } from './MergedUpdateChip'

export function AgyUpdateChip({ control }: { control?: DialogControl } = {}) {
  const t = useT()
  const plan = useAgyPlan()
  const busy = useAgyBusy()
  const installAgy = useStore((s) => s.installAgy)
  const [localOpen, setLocalOpen] = useState(false)
  const open = control ? control.open : localOpen
  const setOpen = (v: boolean) => (control && !v ? control.close() : setLocalOpen(v))
  useUpdateChipRequest((k) => k === 'agy', () => { if (!control) setLocalOpen(true) })
  const range = changelogRange(plan?.hosts ?? [])
  const review = useReviewAsk('agy', range.host, range.from, plan?.target ?? null)
  if (!plan) return null
  const label = busy ? t('agy 更新中…') : agyChipLabel(plan)
  return (
    <>
      {control ? null : (
        <button
          type="button"
          className={`quota-update${busy ? ' running' : ' install'}`}
          disabled={busy}
          title={label}
          aria-label={label}
          onClick={() => setOpen(true)}
        >
          <span aria-hidden="true"><UpgradeIcon /></span>
          <span className="quota-update-n herdr-chip-tag" aria-hidden="true">agy</span>
        </button>
      )}
      <ConfirmDialog
        open={open}
        title={agyUpdateTitle(plan.target)}
        confirmLabel={t('更新')}
        {...review.dialog}
        width={420}
        body={
          <>
            {open ? <UpstreamChangelogBox kind="agy" host={range.host} from={range.from} to={plan.target} refreshKey={review.reviewKey} /> : null}
            {plan.hosts.map((h) => (
              <p key={h.host}>
                <AgyUpdateBody host={h.host} current={h.from} target={plan.target} />
              </p>
            ))}
          </>
        }
        onCancel={() => setOpen(false)}
        onConfirm={() => {
          setOpen(false)
          for (const h of plan.hosts) void installAgy(h.host)
        }}
      />
    </>
  )
}
