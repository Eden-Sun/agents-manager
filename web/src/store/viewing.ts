import type { ShellView } from './store'

export interface ViewSelection {
  selectedBotId: string | null
  selectedProjectId: string | null
  /** 只看有沒有（掛上去就是蓋住整個主畫面），型別照 store 那一份，免得傳錯東西進來還編得過。 */
  shellView: ShellView | null
}

export function viewingBot(s: ViewSelection, botId: string): boolean {
  return s.selectedBotId === botId && !s.selectedProjectId && !s.shellView
}
