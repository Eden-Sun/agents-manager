/**
 * 「使用者現在看得到哪一段對話」——已讀判斷唯一的依據，對齊 `App.tsx` 的畫面分支：
 * `shellView && (群組 || 沒選 bot)` 會把整個主畫面換成終端機，所以 shell 一掛上，
 * bot 對話與群組時間軸都不在畫面上。
 *
 * bot 側本來就有這條守衛，群組側漏掉（issue #509）：選著專案時從側欄點一顆 pane、或專案 ⋯ 的
 * 「在這裡開 shell」，都只設 `shellView`、不清 `selectedProjectId`，群組回覆因此被當成「正在看」
 * 而直接標成已讀——徽章不會亮，已讀標記還被推到最後一則。兩條規則放同一個檔，免得再分岔。
 */

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

export function viewingGroup(s: ViewSelection, projectId: string): boolean {
  return s.selectedProjectId === projectId && !s.shellView
}
