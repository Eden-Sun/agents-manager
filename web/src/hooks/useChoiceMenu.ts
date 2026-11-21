import { useMemo, useRef } from 'react'
import { isCodexUpdateMenu } from '../lib/codexUpdatePrompt'
import { parseChoiceMenu, type TuiChoiceMenu } from '../lib/tuiChoices'
import { useStore } from '../store/store'

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
