import { useMemo, useRef } from 'react'
import { isCodexUpdateMenu } from '../lib/codexUpdatePrompt'
import { parseChoiceMenu, type TuiChoiceMenu } from '../lib/tuiChoices'
import { useStore } from '../store/store'

/**
 * 這張終端快照上有沒有一份可以點的編號選單（`lib/tuiChoices.ts`）。
 *
 * 獨立成 hook 而不是留在 `BlockedChoices` 裡，是因為外面那層（`BlockedPanel`）也要知道答案
 * ——認出選單之後終端快照就預設收起來，兩邊必須是同一個判斷。
 *
 * Codex 的互動升級選單有專屬的 `CodexUpdateHint`（多一顆「先看改了什麼」），讓給它；非互動方框
 * 不得遮住當下真正開著的其他選單。
 */
/**
 * 終端在換頁、重畫的那幾百毫秒裡常常解析不出選單；畫面一閃就卸掉整個問卷（它的預載、已選的答案都跟著丟）。
 * 所以解析不出時短暫沿用上一份（同一顆 bot），超過 `STICKY_MS` 才真的沒有。
 */
const STICKY_MS = 2500

export function useChoiceMenu(botId: string, text: string | null | undefined): TuiChoiceMenu | null {
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? null)
  const parsed = useMemo(() => {
    if (kind === 'codex' && isCodexUpdateMenu(text)) return null
    return parseChoiceMenu(text)
  }, [kind, text])
  const last = useRef<{ botId: string; menu: TuiChoiceMenu; at: number } | null>(null)
  if (parsed) {
    last.current = { botId, menu: parsed, at: Date.now() }
    return parsed
  }
  const prev = last.current
  if (prev && prev.botId === botId && Date.now() - prev.at < STICKY_MS) return prev.menu
  return null
}
