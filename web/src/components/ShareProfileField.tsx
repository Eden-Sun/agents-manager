import type { BotKind } from '../api/types'
import { shareProfileBlocked } from '../lib/shareProfile'
import './botShare.css'

/** 建 bot 表單的「分享用（受限）」：建好之後不能切換，要分享就新建一顆。 */
export function ShareProfileField({ kind, host, value, onChange }: { kind: BotKind; host: string; value: boolean; onChange: (v: boolean) => void }) {
  const blocked = shareProfileBlocked(kind, host)
  return (
    <div className="field share-profile-field">
      <span>用途</span>
      <label title={blocked ?? undefined}>
        <input type="checkbox" checked={value && !blocked} disabled={Boolean(blocked)} onChange={(e) => onChange(e.target.checked)} />
        分享用（受限）
      </label>
      <span className="hint">
        {blocked ??
          (value
            ? '只能在自己的工作目錄讀寫、不能開子 agent、不帶你的憑證；建好後可在設定裡開分享連結給外部使用者。之後不能改回一般 bot。'
            : '要把這顆 bot 分享給外部使用者時才勾。')}
      </span>
    </div>
  )
}
