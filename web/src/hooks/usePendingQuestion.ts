import { useEffect, useMemo, useState } from 'react'
import * as api from '../api'
import { toPendingQuestionsFromResponse, type PendingQuestion } from '../lib/pendingQuestion'
import { useStore } from '../store/store'

export interface PendingQuestionSnapshot {
  questions: PendingQuestion[]
  status: 'loading' | 'ready' | 'unavailable'
}

export function usePendingQuestion(botId: string, paused = false): PendingQuestionSnapshot {
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? null)
  const scope = useMemo(() => ({ botId, kind, paused }), [botId, kind, paused])
  const [state, setState] = useState<PendingQuestionSnapshot & { scope: object }>({ scope, questions: [], status: 'loading' })

  useEffect(() => {
    if (paused || (kind !== 'claude' && kind !== 'codex')) return
    let alive = true
    let requested = 0
    let applied = 0
    const tick = async () => {
      if (document.visibilityState === 'hidden') return
      const sequence = ++requested
      try {
        const raw = await api.fetchPendingQuestion(botId)
        if (alive && sequence > applied) {
          applied = sequence
          setState({ scope, questions: toPendingQuestionsFromResponse(raw), status: 'ready' })
        }
      } catch {
        if (alive && sequence > applied) {
          applied = sequence
          setState({ scope, questions: [], status: 'unavailable' })
        }
      }
    }
    const onVisible = () => {
      if (document.visibilityState === 'visible') void tick()
    }
    document.addEventListener('visibilitychange', onVisible)
    void tick()
    const t = setInterval(() => void tick(), 5000)
    return () => {
      alive = false
      document.removeEventListener('visibilitychange', onVisible)
      clearInterval(t)
    }
  }, [botId, kind, paused, scope])

  return state.scope === scope && (kind === 'claude' || kind === 'codex') && !paused
    ? state
    : { questions: [], status: 'loading' }
}
