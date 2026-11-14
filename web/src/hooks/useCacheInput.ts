import { useShallow } from 'zustand/react/shallow'
import { useStore, composerState } from '../store/store'
import { useCacheTick } from './useCacheTick'
import type { CacheInput } from '../lib/composerCost'
import { effectiveModelOf } from '../lib/codexContextWindow'
import { useApiLastCall } from './useApiTurnCosts'

/** 一顆 bot 的快取狀態輸入（`lib/composerCost`）：run 的欄位＋每 15 秒換一次的「現在」。 */
export function useCacheInput(botId: string): CacheInput {
  const run = useStore(
    useShallow((s) => {
      const r = s.runs[botId]
      const bot = s.bots.find((b) => b.id === botId)
      return {
        kind: bot?.kind ?? null,
        // 有效模型：正在跑的那一份優先，沒有才用設定值。
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
  // claude-api：冷掉時估「下一則要重寫多少」用最近一次呼叫讀到的整段 context（input＋cache_read＋cache_creation）。
  const last = useApiLastCall(run.kind === 'claude-api' ? botId : undefined)
  const apiLastTokens = last ? last.input_tokens + last.cache_read_tokens + last.cache_creation_tokens : null
  const now = useCacheTick(Boolean(run.ttlSecs || run.promptCache))
  return { ...run, apiLastTokens, nowMs: now }
}
