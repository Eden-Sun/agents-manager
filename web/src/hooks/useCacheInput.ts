import { useShallow } from 'zustand/react/shallow'
import { useStore, composerState } from '../store/store'
import { useCacheTick } from './useCacheTick'
import type { CacheInput } from '../lib/composerCost'
import { effectiveModelOf } from '../lib/codexContextWindow'
import { useApiLastCall } from './useApiTurnCosts'

export function useCacheInput(botId: string): CacheInput {
  const run = useStore(
    useShallow((s) => {
      const r = s.runs[botId]
      const bot = s.bots.find((b) => b.id === botId)
      return {
        kind: bot?.kind ?? null,
        model: effectiveModelOf(r?.runtime_model, bot?.model),
        promptCache: r?.prompt_cache ?? null,
        status: r?.status ?? null,
        lastApiAt: r?.last_api_at ?? null,
        keptWarmAt: r?.cache_kept_warm_at ?? null,
        ttlSecs: r?.cache_ttl_secs ?? null,
        working: r?.agent_status === 'working' || composerState(s, botId).inFlightTurnId !== null,
      }
    }),
  )
  const last = useApiLastCall(run.kind === 'claude-api' ? botId : undefined)
  const apiLastTokens = last ? last.input_tokens + last.cache_read_tokens + last.cache_creation_tokens : null
  const now = useCacheTick(Boolean(run.ttlSecs || run.promptCache))
  return { ...run, apiLastTokens, nowMs: now }
}
