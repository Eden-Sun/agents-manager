import { useEffect, useState } from 'react'
import type { OpenAiApiBalance } from '../api/openaiApiBalance'
import { fetchOpenAiApiBalance } from '../api/openaiApiBalance'

let balance: OpenAiApiBalance | null = null
let generation = 0
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
  const gen = generation
  let next: OpenAiApiBalance | null
  try {
    next = await fetchOpenAiApiBalance()
  } catch {
    next = null
  }
  if (gen !== generation) return
  balance = next
  for (const l of listeners) l()
}

export function resetOpenAiApiBalanceForTest() {
  generation++
  balance = null
  listeners.clear()
}
