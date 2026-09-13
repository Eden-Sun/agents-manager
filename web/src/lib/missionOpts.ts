/**
 * 群組「交給 AGM」開任務時的三個選項（`docs/goals/agm-missions.md` §11）與它們的記憶。
 *
 * 三個都是**開任務時就要決定**的事，不是偏好設定：交付方式（D2）、執行者 kind（D7）、
 * 5h 撞限等或換（D5）。記在 localStorage 的只是「上次選了什麼」，下一個任務沿用——同一個人
 * 連開幾個任務多半是同一種交付方式；但仍然每次都攤在眼前，不做成藏起來的預設值。
 */
import type { BotKind, MissionDelivery, MissionOn5h } from '../api/types'

export interface MissionOpts {
  delivery_mode: MissionDelivery
  executor_kind: BotKind
  on_5h_limit: MissionOn5h
}

export const MISSION_OPTS_DEFAULT: MissionOpts = {
  delivery_mode: 'pr',
  executor_kind: 'claude',
  on_5h_limit: 'wait',
}

const KEY = 'am.mission.opts'

export function loadMissionOpts(): MissionOpts {
  try {
    const raw = localStorage.getItem(KEY)
    if (!raw) return MISSION_OPTS_DEFAULT
    const v = JSON.parse(raw) as Partial<MissionOpts>
    return {
      delivery_mode: v.delivery_mode === 'push_main' ? 'push_main' : 'pr',
      executor_kind: v.executor_kind === 'codex' || v.executor_kind === 'grok' ? v.executor_kind : 'claude',
      on_5h_limit: v.on_5h_limit === 'switch' ? 'switch' : 'wait',
    }
  } catch {
    return MISSION_OPTS_DEFAULT
  }
}

export function saveMissionOpts(opts: MissionOpts): void {
  try {
    localStorage.setItem(KEY, JSON.stringify(opts))
  } catch {
    // 無痕視窗之類的寫不進去；預設值照樣能用，不值得為它跳錯誤。
  }
}

