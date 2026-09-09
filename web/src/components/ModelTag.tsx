import { useStore } from '../store/store'
import { effortLabel } from '../api/types'
import { shortModel } from '../lib/shortModel'
import { driftTitle, runtimeDrift, runtimeKnown } from '../lib/runtimeDrift'

/**
 * `opus · 高` — the model a bot is on, with its reasoning effort next to it.
 *
 * Effort belongs next to the model name because the two are one setting in practice: the same
 * `opus` at `低` and at `高` behave nothing alike, and the model name alone cannot tell them
 * apart. The separator is `-` (`opus-高`) **because the user asked for it by name** — it was
 * briefly changed to `·`, and then to nothing at all (`grok-4.6中`), so: do not change it back
 * without asking them. `fast` / `thinking` stay in the tooltip: these rows are narrow enough
 * that a third segment gets ellipsized away.
 *
 * The effort shown is what the CLI **reported** when it has said (the statusLine payload),
 * falling back to what the bot is configured with before it has ever run.
 */
export function ModelTag({ botId }: { botId: string }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const run = useStore((s) => s.runs[botId] ?? null)
  const reported = useStore((s) => s.runs[botId]?.status ?? null)
  if (!bot) return null

  // SPEC §4.4a：這一格跟標題列、跟 codex 自己印的狀態列講的必須是同一件事——**現在在跑什麼**。
  // 所以順序是：CLI 自己報的（claude 的 statusLine）→ 這個 run 啟動時真的吃到的
  // （`run.runtime_*`）→ 都沒有才退回 `bots` 的設定。設定改了還沒重啟時多標一顆 ⟳，重啟後
  // 會變成什麼寫在 tooltip 裡——側欄這一列窄到放不下兩組數字。
  const drift = runtimeDrift(bot, run)
  const live = runtimeKnown(run)
  const effort = reported?.effort ?? (live ? run!.runtime_effort : bot.effort)
  const chipExtra = effort ? effortLabel(effort) : ''
  const fast = reported?.fast_mode ?? (live ? (run!.runtime_fast ?? bot.fast) : bot.fast)
  const detail = [chipExtra, fast ? 'fast' : null, reported?.thinking ? 'thinking' : null].filter(Boolean).join(' · ')

  // `bot.model` is null when the bot is left on whatever the CLI picks. Once the CLI has
  // said what that is (statusLine `model_name`), show it — the same thing the chat header
  // shows — instead of a literal `CLI 預設` that reads as a different model (2026-09-08).
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
