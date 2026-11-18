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
  // 讀不到就當沒有（晶片不畫），不能讓額度列掛一個沒人接的 rejection。
  try {
    balance = await fetchOpenAiApiBalance()
  } catch {
    balance = null
  }
  for (const l of listeners) l()
}

/** 測試用：清掉模組快取，讓下一個測試從頭讀。 */
export function resetOpenAiApiBalanceForTest() {
  balance = null
  listeners.clear()
}
