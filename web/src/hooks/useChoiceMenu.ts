import { useMemo } from 'react'
import { parseCodexUpdatePrompt } from '../lib/codexUpdatePrompt'
import { parseChoiceMenu, type TuiChoiceMenu } from '../lib/tuiChoices'
import { useStore } from '../store/store'

/**
 * 這張終端快照上有沒有一份可以點的編號選單（`lib/tuiChoices.ts`）。
 *
 * 獨立成 hook 而不是留在 `BlockedChoices` 裡，是因為外面那層（`BlockedPanel`）也要知道答案
 * ——認出選單之後終端快照就預設收起來，兩邊必須是同一個判斷。
 *
 * codex 的升級提示雖然也是 `> 1. / 2. / 3.`，但那個畫面有專屬的 `CodexUpdateHint`（多一顆
 * 「先看改了什麼」），讓給它，免得同一件事長出兩排按鈕。
 */
export function useChoiceMenu(botId: string, text: string | null | undefined): TuiChoiceMenu | null {
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? null)
  return useMemo(() => {
    if (kind === 'codex' && parseCodexUpdatePrompt(text)) return null
    return parseChoiceMenu(text)
  }, [kind, text])
}
