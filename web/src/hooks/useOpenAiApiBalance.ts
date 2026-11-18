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
  balance = await fetchOpenAiApiBalance()
  for (const l of listeners) l()
}
