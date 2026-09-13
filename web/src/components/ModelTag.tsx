import { useStore } from '../store/store'
import { effortLabel } from '../api/types'
import { shortModel } from '../lib/shortModel'
import { driftTitle, runtimeDrift, runtimeKnown } from '../lib/runtimeDrift'

/**
 * The model a bot is on, with its reasoning effort. Separator `-` (`opus-高`) is user-specified — do not
 * change it without asking. `fast` / `thinking` stay in the tooltip: a third segment gets ellipsized.
 */
export function ModelTag({ botId }: { botId: string }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const run = useStore((s) => s.runs[botId] ?? null)
  const reported = useStore((s) => s.runs[botId]?.status ?? null)
  if (!bot) return null

  // SPEC §4.4a：顯示現在在跑什麼——CLI 回報 → `run.runtime_*` → `bots` 設定。
  const drift = runtimeDrift(bot, run)
  const live = runtimeKnown(run)
  const effort = reported?.effort ?? (live ? run!.runtime_effort : bot.effort)
  const chipExtra = effort ? effortLabel(effort) : ''
  const fast = reported?.fast_mode ?? (live ? (run!.runtime_fast ?? bot.fast) : bot.fast)
  const detail = [chipExtra, fast ? 'fast' : null, reported?.thinking ? 'thinking' : null].filter(Boolean).join(' · ')

  // Null model = CLI default: show the reported `model_name`, not `CLI 預設` that reads as another model (2026-09-08).
  const runningModel = live ? run!.runtime_model : bot.model
  const shown = shortModel(bot.kind, runningModel ?? reported?.model_name ?? null)
  if (!shown && !chipExtra) return null

  const title = `模型：${runningModel ?? (shown ? `CLI 預設，實際載入 ${shown}` : '（CLI 預設）')}${detail ? ` · ${detail}` : ''}`

  return (
    <span
      className={`model-tag${runningModel ? '' : ' reported'}${drift.length ? ' stale' : ''}`}
      title={drift.length ? `${title}\n\n需重啟才生效：\n${driftTitle(drift)}` : title}
    >
      {shown ?? 'CLI 預設'}
      {/* 分隔符在 CSS 的 `.model-tag-extra::before`，不要在這裡再加一個。 */}
      {chipExtra ? <span className="model-tag-extra">{chipExtra}</span> : null}
      {drift.length ? (
        <span className="model-tag-stale" aria-label="需重啟才生效">
          ⟳
        </span>
      ) : null}
    </span>
  )
}
