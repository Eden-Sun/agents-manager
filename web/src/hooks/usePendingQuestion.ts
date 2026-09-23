import { useEffect, useState } from 'react'
import * as api from '../api'
import { toPendingQuestions, type PendingQuestion } from '../lib/pendingQuestion'
import { useStore } from '../store/store'

/**
 * claude 停著等回答時，從 daemon 讀 transcript 裡那一題（`lib/pendingQuestion.ts`）。
 * 5 秒重讀一次：答完（或換下一題）就換掉；讀不到就當沒有，不擋畫面上原本的按鍵面板。
 */
export function usePendingQuestion(botId: string, paused = false): PendingQuestion[] {
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? null)
  const [state, setState] = useState<{ botId: string; questions: PendingQuestion[] }>({ botId, questions: [] })

  useEffect(() => {
    if (paused || kind !== 'claude') return
    let alive = true
    const tick = async () => {
      try {
        const raw = await api.fetchPendingQuestion(botId)
        if (alive) setState({ botId, questions: toPendingQuestions(raw) })
      } catch {
        // 讀不到不是紅字：這只是補充，畫面本身照舊。
      }
    }
    void tick()
    const t = setInterval(() => void tick(), 5000)
    return () => {
      alive = false
      clearInterval(t)
    }
  }, [botId, kind, paused])

  // 換 bot 時舊的那份不能算這顆的。
  return state.botId === botId && kind === 'claude' ? state.questions : []
}
