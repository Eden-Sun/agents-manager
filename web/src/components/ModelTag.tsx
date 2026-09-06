import { useStore } from '../store/store'
import { effortLabel } from '../api/types'

/**
 * `opus-高` — the model a bot is on, with its reasoning effort bound to it.
 *
 * Effort belongs next to the model name because the two are one setting in practice: the same
 * `opus` at `低` and at `高` behave nothing alike, and the model name alone cannot tell them
 * apart. `-` joins them for that reason; `·` stays the separator between *different* facts,
 * which is why `fast` / `thinking` live in the tooltip rather than on the chip — these rows
 * are narrow enough that a third segment gets ellipsized away.
 *
 * The effort shown is what the CLI **reported** when it has said (the statusLine payload),
 * falling back to what the bot is configured with before it has ever run.
 */
export function ModelTag({ botId }: { botId: string }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const reported = useStore((s) => s.runs[botId]?.status ?? null)
  if (!bot) return null

  const effort = reported?.effort ?? bot.effort ?? null
  const chipExtra = effort ? effortLabel(effort) : ''
  const detail = [chipExtra, (reported?.fast_mode ?? bot.fast) ? 'fast' : null, reported?.thinking ? 'thinking' : null]
    .filter(Boolean)
    .join(' · ')

  // `bot.model` is null when the bot is left on whatever the CLI picks; that used to render
  // nothing at all, which hid the effort too.
  if (!bot.model && !chipExtra) return null

  return (
    <span className="model-tag" title={`模型：${bot.model ?? '（CLI 預設）'}${detail ? ` · ${detail}` : ''}`}>
      {bot.model ?? 'CLI 預設'}
      {chipExtra ? <span className="model-tag-extra">-{chipExtra}</span> : null}
    </span>
  )
}
