import { useEffect, useMemo, useState } from 'react'
import * as api from '../api'
import { toPendingQuestionsFromResponse, type PendingQuestion } from '../lib/pendingQuestion'
import { useStore } from '../store/store'

export interface PendingQuestionSnapshot {
  questions: PendingQuestion[]
  status: 'loading' | 'ready' | 'unavailable'
}

/**
 * claude 停著等回答時，從 daemon 讀 transcript 裡那一題（`lib/pendingQuestion.ts`）；codex 的非同步提問（issue #1879）
 * 同一支 API 回 rollout 裡的題目（畫面上只剩 `? 1 question`）。
 * 5 秒重讀一次：答完（或換下一題）就換掉；讀不到則標為無法確認，不擋畫面上原本的按鍵面板。
 * 回應裡的 `jev_options`（Jev 的選項建議，issue #1475）掛在對應那一題的 `jev`；daemon 問完會發 WS `pending_question_hint`，
 * 網頁不另外訂它——下一輪 5 秒輪詢就帶到。
 */
export function usePendingQuestion(botId: string, paused = false): PendingQuestionSnapshot {
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind ?? null)
  // 重新啟用或切換 bot/kind 都是一份新快照，不重用上一輪資料。
  const scope = useMemo(() => ({ botId, kind, paused }), [botId, kind, paused])
  const [state, setState] = useState<PendingQuestionSnapshot & { scope: object }>({ scope, questions: [], status: 'loading' })

  useEffect(() => {
    if (paused || (kind !== 'claude' && kind !== 'codex')) return
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
          setState({ scope, questions: toPendingQuestionsFromResponse(raw), status: 'ready' })
        }
      } catch {
        // 無法確認待答狀態，題目與建議同時失效；晚到的失敗也不能覆蓋新結果。
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

  // 換 bot 時舊的那份不能算這顆的。
  return state.scope === scope && (kind === 'claude' || kind === 'codex') && !paused
    ? state
    : { questions: [], status: 'loading' }
}
