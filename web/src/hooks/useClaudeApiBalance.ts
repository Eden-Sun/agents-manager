import { useEffect, useState } from 'react'
import { fetchClaudeApiBalance, type ClaudeApiBalance } from '../api/claudeApiBalance'

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
  } finally {
    inflight = false
  }
}

export function reloadClaudeApiBalance() {
  return load()
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

export function resetClaudeApiBalanceForTest() {
  latest = null
  inflight = false
  subs.clear()
  if (timer !== null) clearInterval(timer)
  timer = null
}
