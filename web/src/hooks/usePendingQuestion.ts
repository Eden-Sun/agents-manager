import { useEffect, useState } from 'react'
import * as api from '../api'
import { toPendingQuestionsFromResponse, type PendingQuestion } from '../lib/pendingQuestion'
import { useStore } from '../store/store'

/**
 * claude 停著等回答時，從 daemon 讀 transcript 裡那一題（`lib/pendingQuestion.ts`）。
 * 5 秒重讀一次：答完（或換下一題）就換掉；讀不到就當沒有，不擋畫面上原本的按鍵面板。
 * 回應裡的 `jev_options`（Jev 的選項建議，issue #1475）掛在對應那一題的 `jev`；daemon 問完會發 WS `pending_question_hint`，
 * 網頁不另外訂它——下一輪 5 秒輪詢就帶到。
 */
export function usePendingQuestion(botId: string, paused = false): PendingQuestion[] {
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? null)
  const [state, setState] = useState<{ botId: string; questions: PendingQuestion[] }>({ botId, questions: [] })

  useEffect(() => {
    if (paused || kind !== 'claude') return
    let alive = true
    let requested = 0
    let applied = 0
    const tick = async () => {
      // 分頁在背景就不讀；回前景時 `onVisible` 立刻補一次。
      if (document.visibilityState === 'hidden') return
      const sequence = ++requested
      try {
        const raw = await api.fetchPendingQuestion(botId)
        // 比已套用的回應新就接受；後續請求尚未完成不會餓死慢請求。
        if (alive && sequence > applied) {
          applied = sequence
          setState({ botId, questions: toPendingQuestionsFromResponse(raw) })
        }
      } catch {
        // 讀不到不是紅字：這只是補充，畫面本身照舊。
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
  }, [botId, kind, paused])

  // 換 bot 時舊的那份不能算這顆的。
  return state.botId === botId && kind === 'claude' ? state.questions : []
}
