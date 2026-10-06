/**
 * 主力的「保溫」（SPEC §6.5k）在網頁端的認法：
 * 保溫＝cache 58 分時 daemon 代送的那則 `any updates`、保溫回覆＝bot 對它的回覆；熱壓＝110 分的壓縮；涼掉＝過了 cache TTL。
 * 保溫回合的訊息帶 `keep_warm: true`，回合的 `client_request_id` 以 `keep-warm:` 開頭（DB 舊資料是 `keepalive:`，兩種都認）；
 * 兩者都不算未讀（`store/unread.ts`、`store/store.ts`）。
 */
import type { Bot, Run } from '../api/types'
import { cacheState } from './cacheClock'

const KEEP_WARM_REQUEST_PREFIXES = ['keep-warm:', 'keepalive:']

/** 回合的 `client_request_id` 是不是保溫回合的（含舊前綴）。 */
export function isKeepWarmRequestId(clientRequestId: string | null | undefined): boolean {
  return Boolean(clientRequestId) && KEEP_WARM_REQUEST_PREFIXES.some((p) => clientRequestId!.startsWith(p))
}

/** 「不用保溫」鈕只給主力的 claude／codex（grok 沒有 cache 倒數、也不保溫）。 */
export function keepWarmSkippable(bot: Pick<Bot, 'primary' | 'kind'> | undefined): boolean {
  return Boolean(bot?.primary) && (bot?.kind === 'claude' || bot?.kind === 'codex')
}

/**
 * 保溫回覆到了且快取尚未涼掉、使用者還沒送新 prompt：主力晶片框換色（`keepWarmChip.css`）。
 * 保溫的效果只有一個 cache TTL（照 cacheClock 既有的 TTL 與 cache_kept_warm_at／last_api_at 判斷），
 * cache 一涼掉框與 ♨ 就消失；使用者送 prompt（keep_warm_replied_at 清為 null）仍照舊立即清除。
 */
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

