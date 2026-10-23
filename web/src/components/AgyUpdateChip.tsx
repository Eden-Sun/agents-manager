import { useState } from 'react'
import { useStore } from '../store/store'
import { useAgyBusy, useAgyPlan } from '../hooks/useAgyPlan'
import { agyChipLabel, agyUpdateTitle } from '../lib/agyUpdate'
import { changelogRange } from '../lib/upstreamRange'
import { useReviewAsk } from '../hooks/useReviewAsk'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { AgyUpdateBody } from './AgyUpdateConfirm'
import { UpstreamChangelogBox } from './UpstreamChangelogBox'
import type { DialogControl } from './MergedUpdateChip'

/**
 * header 的「agy 有新版」（issue #1921）：跟 grok／herdr 同一個位置與樣子。按下去跟設定頁「更新檢查」同一個確認框
 * （文字在 `AgyUpdateConfirm.tsx`），確認後每台落後的主機各跑一次 `installAgy`（`POST /api/hosts/{name}/agy/install`）。
 * 有主機正在裝時鈕停用、字改成「agy 更新中…」。
 */
export function AgyUpdateChip({ control }: { control?: DialogControl } = {}) {
  const plan = useAgyPlan()
  const busy = useAgyBusy()
  const installAgy = useStore((s) => s.installAgy)
  const [localOpen, setLocalOpen] = useState(false)
  const open = control ? control.open : localOpen
  const setOpen = (v: boolean) => (control && !v ? control.close() : setLocalOpen(v))
  const range = changelogRange(plan?.hosts ?? [])
  const review = useReviewAsk('agy', range.host, range.from, plan?.target ?? null)
  if (!plan) return null
  const label = busy ? 'agy 更新中…' : agyChipLabel(plan)
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
        confirmLabel="更新"
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
