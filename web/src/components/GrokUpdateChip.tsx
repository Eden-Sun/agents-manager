import { useState } from 'react'
import { useGrokPlan } from '../hooks/useGrokPlan'
import { grokChipLabel } from '../lib/grokUpdate'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'

/**
 * header 的「grok 有新版」（issue #761）：跟 claude／codex／herdr 那幾顆同一個位置與樣子，但**只提示、不一鍵安裝**——
 * grok 的升級是在那台主機上跑 `grok update`（官方 installer，磁碟換版後跑著的 bot 要重啟才換），沒有我們能代跑的固定流程。
 * 持續顯示到每台主機都追上為止；按下去只是把指令與落後的主機講清楚。
 */
export function GrokUpdateChip() {
  const plan = useGrokPlan()
  const [open, setOpen] = useState(false)
  if (!plan) return null
  const label = grokChipLabel(plan)
  return (
    <>
      <button type="button" className="quota-update waiting" title={label} aria-label={label} onClick={() => setOpen(true)}>
        <span aria-hidden="true"><UpgradeIcon /></span>
        <span className="quota-update-n herdr-chip-tag" aria-hidden="true">grok</span>
      </button>
      <ConfirmDialog
        open={open}
        title={`grok 有新版 ${plan.target}`}
        confirmLabel="知道了"
        cancelLabel="關閉"
        body={
          <>
            <p>這些主機的 grok 還不是最新的正式版：</p>
            <ul className="confirm-choices">
              {plan.hosts.map((h) => (
                <li key={h.host}>
                  <strong>{h.host}</strong>：{h.from ?? '讀不到版本'} → {plan.target}
                </li>
              ))}
            </ul>
            <p className="confirm-note">
              到那台主機執行 <code>{plan.command}</code>（grok 官方的升級指令）。這裡不代為安裝；裝好之後跑著的 grok bot 要重啟才會換成新版。
            </p>
          </>
        }
        onConfirm={() => setOpen(false)}
        onCancel={() => setOpen(false)}
      />
    </>
  )
}
