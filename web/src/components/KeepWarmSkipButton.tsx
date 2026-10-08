import { useState } from 'react'
import * as api from '../api'
import { ApiError } from '../api/types'
import { useStore } from '../store/store'
import { keepWarmSkippable } from '../lib/keepWarm'
import './compactButton.css'

/**
 * 「壓縮」鈕旁的「不用保溫」開關（主力的 claude／codex 才有）：開著＝這顆 bot「這一輪閒置」跳過保溫與熱壓，
 * 有真的活動（使用者或 bot 新回合，不含保溫／熱壓本身）daemon 自動恢復並廣播；再按一次取消。狀態在 `run.keep_warm_skip`。
 */
export function KeepWarmSkipButton({ botId, touch = false }: { botId: string; touch?: boolean }) {
  const eligible = useStore((s) => keepWarmSkippable(s.bots.find((b) => b.id === botId)))
  const skipping = useStore((s) => s.runs[botId]?.keep_warm_skip === true)
  const hasRun = useStore((s) => Boolean(s.runs[botId]))
  const notify = useStore((s) => s.notify)
  const [busy, setBusy] = useState(false)
  if (!eligible || !hasRun) return null
  const go = async () => {
    setBusy(true)
    try {
      const { keep_warm_skip } = await api.setKeepWarmSkip(botId, !skipping)
      // daemon 會廣播 run 更新；先套一份，免得慢一拍才看到按鈕換狀態。
      useStore.setState((s) => {
        const run = s.runs[botId]
        return run ? { runs: { ...s.runs, [botId]: { ...run, keep_warm_skip } } } : {}
      })
      notify('info', keep_warm_skip ? '這一輪閒置不保溫、也不熱壓；有新活動就自動恢復' : '已取消，照常保溫')
    } catch (e) {
      const reason = e instanceof ApiError ? String(e.body.error ?? e.message) : String(e)
      notify('error', reason === 'not_primary' ? '只有主力的 claude／codex 能設定不用保溫' : `沒有切換：${reason}`)
    } finally {
      setBusy(false)
    }
  }
  return (
    <button
      type="button"
      className={`compact-btn keep-warm-skip-btn${touch ? ' touch' : ''}${skipping ? ' on' : ''}`}
      aria-pressed={skipping}
      disabled={busy}
      title={
        skipping
          ? '已設定不用保溫：這一輪閒置不保溫、也不熱壓；有新活動就自動恢復。點一下取消'
          : '這一輪閒置不要保溫（58 分的 any updates）與熱壓；有新活動就自動恢復'
      }
      onClick={(e) => {
        e.stopPropagation()
        void go()
      }}
    >
      {skipping ? '不保溫中・取消' : '不用保溫'}
    </button>
  )
}

/**
 * 手機版的「不用保溫」入口（2026-10-08 使用者：「手機版 也要能 設定不保溫」）：手機的狀態列收在「Git / 專案資訊」彈窗裡，
 * 而且 context 項旁那顆 16px 高的小鈕點不到，所以彈窗最上面另給一列、觸控目標 ≥ 40px。同一顆元件、同一支 API。
 */
export function MobileKeepWarmRow({ botId }: { botId: string }) {
  const eligible = useStore((s) => keepWarmSkippable(s.bots.find((b) => b.id === botId)))
  const hasRun = useStore((s) => Boolean(s.runs[botId]))
  if (!eligible || !hasRun) return null
  return (
    <div className="mobile-keep-warm-row">
      <span className="mobile-keep-warm-label">快取保溫</span>
      <KeepWarmSkipButton botId={botId} touch />
    </div>
  )
}
