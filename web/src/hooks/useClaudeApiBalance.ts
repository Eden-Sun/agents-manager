import { useEffect, useState } from 'react'
import { fetchClaudeApiBalance, type ClaudeApiBalance } from '../api/claudeApiBalance'

/**
 * 全域額度列的 Anthropic API 餘額（`GET /api/claude-api/balance`）：所有訂閱者共用一份、每分鐘更新一次，
 * 沒人訂閱就不打（daemon 端 cost_report 另有 10 分鐘快取，這裡不會打爆）。讀不到＝保留上一份；一次都沒讀到＝null。
 */
const POLL_MS = 60_000
let latest: ClaudeApiBalance | null = null
let timer: ReturnType<typeof setInterval> | null = null
let inflight = false
const subs = new Set<(b: ClaudeApiBalance | null) => void>()

async function load() {
  if (inflight) return
  inflight = true
  try {
    latest = await fetchClaudeApiBalance()
    for (const s of subs) s(latest)
  } catch {
    // 讀不到就沿用上一份
  } finally {
    inflight = false
  }
}

export function useClaudeApiBalance(): ClaudeApiBalance | null {
  const [b, setB] = useState<ClaudeApiBalance | null>(latest)
  useEffect(() => {
    subs.add(setB)
    if (timer === null) timer = setInterval(() => void load(), POLL_MS)
    void load()
    return () => {
      subs.delete(setB)
      if (subs.size === 0 && timer !== null) {
        clearInterval(timer)
        timer = null
      }
    }
  }, [])
  return b
}

/** 給測試：清掉共用快取。 */
export function resetClaudeApiBalanceForTest() {
  latest = null
  inflight = false
  subs.clear()
  if (timer !== null) clearInterval(timer)
  timer = null
}
