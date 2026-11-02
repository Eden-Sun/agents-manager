import type { CSSProperties } from 'react'
import { composerState, useStore } from '../store/store'
import { chipCache } from '../lib/cacheClock'
import { useCacheTick } from '../hooks/useCacheTick'
import './cacheClock.css'
import { effortLabel, effortShort } from '../api/types'
import { agyEffort, shortModel } from '../lib/shortModel'
import { driftFieldsLabel, driftTitle, runningFast, runtimeDrift, runtimeSettingsKnown } from '../lib/runtimeDrift'
import { useT } from '../lib/i18n'

/**
 * The model a bot is on, with its reasoning effort. Separator `-` (`opus-高`) is user-specified — do not
 * change it without asking. `fast` / `thinking` stay in the tooltip: a third segment gets ellipsized.
 */
export function ModelTag({
  botId,
  short = false,
  cache: withCache = false,
}: {
  botId: string
  /** 精簡列：強度只寫縮寫（全名在 tooltip）。 */
  short?: boolean
  /** 側欄：標籤底色畫快取倒數（同主力晶片 `cacheClock.css`），child 也看得到（使用者 2026-11-02）。 */
  cache?: boolean
}) {
  const t = useT()
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const run = useStore((s) => s.runs[botId] ?? null)
  const reported = useStore((s) => s.runs[botId]?.status ?? null)
  const working = useStore((s) => s.runs[botId]?.agent_status === 'working' || composerState(s, botId).inFlightTurnId !== null)
  const ticking = withCache && Boolean(run?.cache_ttl_secs)
  const now = useCacheTick(ticking)
  const cacheNow = ticking ? chipCache(run, now, working) : null
  const cache = cacheNow && cacheNow.level !== 'cold' ? cacheNow : null
  if (!bot) return null

  // SPEC §4.4a：顯示現在在跑什麼——CLI 回報 → `run.runtime_*` → `bots` 設定；drift 只做欄位說明。
  const drift = runtimeDrift(bot, run)
  const live = runtimeSettingsKnown(run)
  const runningModel = live ? run!.runtime_model : bot.model
  const effort = reported?.effort ?? (live ? run!.runtime_effort : bot.effort) ?? agyEffort(bot.kind, runningModel)
  const chipExtra = effort ? effortLabel(effort) : ''
  const shownExtra = effort ? (short ? effortShort(effort) : chipExtra) : ''
  const fast = runningFast(bot, run)
  const detail = [chipExtra, fast ? 'fast' : null, reported?.thinking ? 'thinking' : null].filter(Boolean).join(' · ')

  // Null model = CLI default: show the reported `model_name`, not `CLI 預設` that reads as another model (2026-09-08).
  const shown = shortModel(bot.kind, runningModel ?? reported?.model_name ?? null)
  if (!shown && !chipExtra) return null

  const modelDesc = runningModel ?? (shown ? t('CLI 預設，實際載入 {shown}', { shown }) : t('（CLI 預設）'))
  const title = `${t('模型：{model}', { model: modelDesc })}${detail ? ` · ${detail}` : ''}`
  const restartLabel = t('需重啟才生效：{fields}', { fields: driftFieldsLabel(drift) })

  return (
    <span
      className={`model-tag${runningModel ? '' : ' reported'}${drift.length ? ' stale' : ''}${cache ? ` cache-${cache.level}` : ''}`}
      style={cache ? ({ '--cache-fill': `${(cache.frac * 100).toFixed(1)}%` } as CSSProperties) : undefined}
      title={`${drift.length ? `${restartLabel}\n${title}\n\n${driftTitle(drift)}` : title}${cache ? `\n${cache.title}` : ''}`}
    >
      {/* 窄列放不下時先截模型名／強度，⟳ 與欄位字（`.model-tag-stale`）留著（#1903）。 */}
      <span className="model-tag-main">
        {shown ?? t('CLI 預設')}
        {/* 分隔符在 CSS 的 `.model-tag-extra::before`，不要在這裡再加一個。 */}
        {shownExtra ? <span className="model-tag-extra">{shownExtra}</span> : null}
      </span>
      {drift.length ? (
        <span className="model-tag-stale" aria-label={restartLabel}>
          ⟳{driftFieldsLabel(drift, short || drift.length > 1)}
        </span>
      ) : null}
    </span>
  )
}
