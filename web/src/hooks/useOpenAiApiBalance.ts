import { useEffect, useState } from 'react'
import type { OpenAiApiBalance } from '../api/openaiApiBalance'
import { fetchOpenAiApiBalance } from '../api/openaiApiBalance'

let balance: OpenAiApiBalance | null = null
const listeners = new Set<() => void>()

export function useOpenAiApiBalance() {
  const [b, setB] = useState(balance)
  useEffect(() => {
    const l = () => setB(balance)
    listeners.add(l)
    if (!balance) void reloadOpenAiApiBalance()
    return () => {
      listeners.delete(l)
    }
  }, [])
  return b
}

export async function reloadOpenAiApiBalance() {
  try {
    balance = await fetchOpenAiApiBalance()
  } catch {
    balance = null
  }
  for (const l of listeners) l()
}

export function resetOpenAiApiBalanceForTest() {
  balance = null
  listeners.clear()
}
