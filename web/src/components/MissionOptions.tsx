/**
 * 群組輸入框的「交給 AGM」：開關＋三個選項（`docs/goals/agm-missions.md` §11）。
 *
 * 三個都是**開任務時就要決定**的事，不是偏好設定：
 * - 交付方式（D2）：直接推 main／開 PR 給我看。
 * - 執行者 kind（D7）：claude／codex／grok；帳號輪換只對 claude 有效。
 * - 5h 撞限（D5）：等重置／不等直接換下一個身分。7d 撞限一律換，沒得選。
 *
 * 選項本身與它的記憶在 `lib/missionOpts.ts`。
 */
import type { MissionOpts } from '../lib/missionOpts'
import { saveMissionOpts } from '../lib/missionOpts'

/** 一組互斥選項。每顆 44px 觸控目標，選到的那顆不只換顏色——文字也會變粗。 */
function Row<T extends string>({
  label,
  hint,
  value,
  options,
  disabled,
  onPick,
}: {
  label: string
  hint?: string
  value: T
  options: { v: T; label: string; title: string }[]
  disabled?: boolean
  onPick: (v: T) => void
}) {
  return (
    <div className="mo-row">
      <span className="mo-label" title={hint}>
        {label}
      </span>
      <div className="mo-choices" role="group" aria-label={label}>
        {options.map((o) => (
          <button
            key={o.v}
            type="button"
            className={`mo-choice${o.v === value ? ' at' : ''}`}
            aria-pressed={o.v === value}
            disabled={disabled}
            title={o.title}
            onClick={() => onPick(o.v)}
          >
            {o.label}
          </button>
        ))}
      </div>
    </div>
  )
}

export function MissionOptions({
  opts,
  disabled,
  onChange,
}: {
  opts: MissionOpts
  disabled?: boolean
  onChange: (next: MissionOpts) => void
}) {
  const set = (patch: Partial<MissionOpts>) => {
    const next = { ...opts, ...patch }
    onChange(next)
    saveMissionOpts(next)
  }
  return (
    <div className="mission-opts-panel">
      <Row
        label="交付"
        hint="做完之後要直接推 main，還是開 PR 讓你看過再合"
        value={opts.delivery_mode}
        options={[
          { v: 'pr', label: '開 PR 給我看', title: '推一個 mission/<id> 分支並開 PR' },
          { v: 'push_main', label: '直接推 main', title: 'rebase origin/main、整樹驗證過才 fast-forward 推上去；失敗會停下來問你' },
        ]}
        disabled={disabled}
        onPick={(v) => set({ delivery_mode: v })}
      />
      <Row
        label="執行者"
        hint="用哪一種 CLI 跑這個任務；撞限自動換帳號只有 claude 有"
        value={opts.executor_kind}
        options={[
          { v: 'claude', label: 'claude', title: '撞限時自動照 cc2 → cc1 → cc0 換身分' },
          { v: 'codex', label: 'codex', title: '沒有身分可以輪換，撞限只能等重置或停下來問你' },
          { v: 'grok', label: 'grok', title: '沒有身分可以輪換，撞限只能等重置或停下來問你' },
        ]}
        disabled={disabled}
        onPick={(v) => set({ executor_kind: v })}
      />
      <Row
        label="撞到 5h 上限"
        hint="7d 上限一律換下一個身分，這裡只管 5h 那一桶"
        value={opts.on_5h_limit}
        options={[
          { v: 'wait', label: '等重置', title: '原地等這個身分的 5h 桶回來再繼續' },
          { v: 'switch', label: '不等，換下一個', title: '立刻換下一個身分接手（同一個 worktree、新的 session）' },
        ]}
        disabled={disabled}
        onPick={(v) => set({ on_5h_limit: v })}
      />
    </div>
  )
}
