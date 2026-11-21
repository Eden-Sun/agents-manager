import type { ShellView } from './store'

export interface ViewSelection {
  selectedBotId: string | null
  selectedProjectId: string | null
  shellView: ShellView | null
}

export function viewingBot(s: ViewSelection, botId: string): boolean {
  return s.selectedBotId === botId && !s.selectedProjectId && !s.shellView
}
