import type { CSSProperties, ReactNode } from 'react'
import { composerState, useStore } from '../store/store'
import { chipCache } from '../lib/cacheClock'
import { useCacheTick } from '../hooks/useCacheTick'
import './cacheClock.css'
import { effortLabel, effortShort } from '../api/types'
import { agyEffort, shortModel } from '../lib/shortModel'
import { driftFieldsLabel, driftTitle, runningFast, runtimeDrift, runtimeSettingsKnown } from '../lib/runtimeDrift'
import { useT } from '../lib/i18n'

export function ModelTag({
  botId,
  short = false,
  cache: withCache = false,
  fallback = null,
}: {
  botId: string
  short?: boolean
  cache?: boolean
  fallback?: ReactNode
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

  const drift = runtimeDrift(bot, run)
  const live = runtimeSettingsKnown(run)
  const runningModel = live ? run!.runtime_model : bot.model
  const effort = reported?.effort ?? (live ? run!.runtime_effort : bot.effort) ?? agyEffort(bot.kind, runningModel)
  const chipExtra = effort ? effortLabel(effort) : ''
  const shownExtra = effort ? (short ? effortShort(effort) : chipExtra) : ''
  const fast = runningFast(bot, run)
  const detail = [chipExtra, fast ? 'fast' : null, reported?.thinking ? 'thinking' : null].filter(Boolean).join(' · ')

  const rawShown = shortModel(bot.kind, runningModel ?? reported?.model_name ?? null)
  const reportedShown = shortModel(bot.kind, reported?.model_name ?? null)
  const shown =
    rawShown && reportedShown && !rawShown.includes('-') && reportedShown.startsWith(`${rawShown}-`)
      ? reportedShown
      : rawShown
  if (!shown && !chipExtra) return fallback

  const modelDesc = runningModel ?? (shown ? t('CLI 預設，實際載入 {shown}', { shown }) : t('（CLI 預設）'))
  const title = `${t('模型：{model}', { model: modelDesc })}${detail ? ` · ${detail}` : ''}`
  const restartLabel = t('需重啟才生效：{fields}', { fields: driftFieldsLabel(drift) })

  return (
    <span
      className={`model-tag${runningModel ? '' : ' reported'}${drift.length ? ' stale' : ''}${cache ? ` cache-${cache.level}` : ''}`}
      style={cache ? ({ '--cache-fill': `${(cache.frac * 100).toFixed(1)}%` } as CSSProperties) : undefined}
      title={`${drift.length ? `${restartLabel}\n${title}\n\n${driftTitle(drift)}` : title}${cache ? `\n${cache.title}` : ''}`}
    >
      {}
      <span className="model-tag-main">
        {shown ?? t('CLI 預設')}
        {}
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
