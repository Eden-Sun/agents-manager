import { useState } from 'react'
import * as api from '../api'
import { ApiError } from '../api/types'
import { useStore } from '../store/store'
import { keepWarmReplied, keepWarmSkippable } from '../lib/keepWarm'
import { useCacheTick } from '../hooks/useCacheTick'
import { useT } from '../lib/i18n'
import './compactButton.css'

/**
 * 「壓縮」鈕旁的「不用保溫」開關（主力的 claude 才有；codex 的 cache 不會冷，不保溫）：開著＝這顆 bot「這一輪閒置」跳過保溫與熱壓，
 * 有真的活動（使用者或 bot 新回合，不含保溫／熱壓本身）daemon 自動恢復並廣播；再按一次取消。狀態在 `run.keep_warm_skip`。
 */
export function KeepWarmSkipButton({ botId, touch = false }: { botId: string; touch?: boolean }) {
  const t = useT()
  const eligible = useStore((s) => keepWarmSkippable(s.bots.find((b) => b.id === botId)))
  const run = useStore((s) => s.runs[botId])
  const skipping = run?.keep_warm_skip === true
  // 與主力晶片的 ♨ 同一判斷；即使沒有 run 廣播，TTL 到期也要換回文案。
  const now = useCacheTick(eligible && !skipping && Boolean(run?.keep_warm_replied_at))
  const warmed = keepWarmReplied(run, now)
  const notify = useStore((s) => s.notify)
  const [busy, setBusy] = useState(false)
  if (!eligible || !run) return null
  const go = async () => {
    setBusy(true)
    try {
      const { keep_warm_skip } = await api.setKeepWarmSkip(botId, !skipping)
      // daemon 會廣播 run 更新；先套一份，免得慢一拍才看到按鈕換狀態。
      useStore.setState((s) => {
        const run = s.runs[botId]
        return run ? { runs: { ...s.runs, [botId]: { ...run, keep_warm_skip } } } : {}
      })
      notify('info', keep_warm_skip ? t('這一輪閒置不保溫、也不熱壓；有新活動就自動恢復') : t('已取消，照常保溫'))
    } catch (e) {
      const reason = e instanceof ApiError ? String(e.body.error ?? e.message) : String(e)
      notify('error', reason === 'not_primary' ? t('只有主力的 claude 能設定不用保溫') : t('沒有切換：{reason}', { reason }))
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
          ? t('已設定不用保溫：這一輪閒置不保溫、也不熱壓；有新活動就自動恢復。點一下取消')
          : warmed
            ? t('保溫回覆已到、快取仍熱；點一下停止這一輪閒置保溫，有新活動就自動恢復')
            : t('這一輪閒置不要保溫（58 分的 any updates）與熱壓；有新活動就自動恢復')
      }
      onClick={(e) => {
        e.stopPropagation()
        void go()
      }}
    >
      {skipping ? t('不保溫中・取消') : warmed ? t('保溫中・停止保溫') : t('不用保溫')}
    </button>
  )
}
