import type { Lamp } from '../api/types'

import { backgroundLabel } from '../lib/backgroundJobs'
import { LAMP_LABEL } from './lampLabel'
import './backgroundJobs.css'

/**
 * `background`：回合結束但背景還有幾個工作在跑（`backgroundJobs`，#714）。只在 idle 燈上有意義：多一圈 accent 色的呼吸環，
 * 預設的 tooltip／aria-label 也改寫「背景執行中（N）」，跟側欄同一個字串。
 */
export function StatusLamp({ lamp, title, background = 0 }: { lamp: Lamp; title?: string; background?: number }) {
  const bg = lamp === 'idle' && background > 0 ? background : 0
  const text = title ?? (bg > 0 ? backgroundLabel(bg) : LAMP_LABEL[lamp])
  return <span className={`lamp lamp-${lamp}${bg > 0 ? ' lamp-bg' : ''}`} role="img" aria-label={text} title={text} />
}
