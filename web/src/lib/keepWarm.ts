import type { Bot, Run } from '../api/types'
import { cacheState } from './cacheClock'

const KEEP_WARM_REQUEST_PREFIXES = ['keep-warm:', 'keepalive:']

export function isKeepWarmRequestId(clientRequestId: string | null | undefined): boolean {
  return Boolean(clientRequestId) && KEEP_WARM_REQUEST_PREFIXES.some((p) => clientRequestId!.startsWith(p))
}

export function keepWarmSkippable(bot: Pick<Bot, 'primary' | 'kind'> | undefined): boolean {
  return Boolean(bot?.primary) && bot?.kind === 'claude'
}

export function keepWarmReplied(
  run: Partial<Pick<Run, 'keep_warm_replied_at' | 'last_api_at' | 'cache_ttl_secs' | 'cache_kept_warm_at' | 'agent_status'>> | null | undefined,
  nowMs: number = Date.now(),
): boolean {
  if (!run?.keep_warm_replied_at) return false
  const cache = cacheState(
    run.last_api_at ?? run.keep_warm_replied_at,
    run.cache_ttl_secs,
    nowMs,
    run.agent_status === 'working',
    run.cache_kept_warm_at ?? run.keep_warm_replied_at,
  )
  return Boolean(cache && cache.level !== 'cold')
}

