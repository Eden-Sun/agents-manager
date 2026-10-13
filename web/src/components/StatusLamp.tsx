import type { Lamp } from '../api/types'

import { backgroundLabel } from '../lib/backgroundJobs'
import { LAMP_LABEL } from './lampLabel'
import { CAPACITY_LABEL } from '../lib/modelCapacity'
import './backgroundJobs.css'
import './modelCapacity.css'

/**
 * `background`：回合結束但背景還有幾個工作在跑（`backgroundJobs`，#714）。只在 idle 燈上有意義：多一圈 accent 色的呼吸環，
 * 預設的 tooltip／aria-label 也改寫「背景執行中（N）」，跟側欄同一個字串。
 */
export function StatusLamp({ lamp, title, background = 0, kids = 0, capacity = false }: { lamp: Lamp; title?: string; background?: number; /** 底下在跑的子 agent 數 */ kids?: number; /** codex 模型滿載（#1674）：閒置燈換紫 */ capacity?: boolean }) {
  const bg = lamp === 'idle' && background > 0 ? background : 0
  // 外圈轉圈只給「自己閒著、子 agent 還在跑」（2026-10-04 使用者：「沒有 child 也在轉，轉個毛」）；只有背景 shell 就是一般綠點，
  // 「背景 N」交給旁邊的字。
  const spin = lamp === 'idle' && kids > 0
  const full = capacity && lamp === 'idle'
  const text = title ?? (full ? CAPACITY_LABEL : spin ? `子 agent 還在跑（${kids}）` : bg > 0 ? backgroundLabel(bg) : LAMP_LABEL[lamp])
  return <span className={`lamp lamp-${lamp}${spin ? ' lamp-bg' : ''}${full ? ' lamp-capacity' : ''}`} role="img" aria-label={text} title={text} />
}
