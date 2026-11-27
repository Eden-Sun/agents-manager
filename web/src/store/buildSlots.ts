import { useEffect, useState } from 'react'
import type { BuildSlots } from '../api/buildSlots'
import { fetchBuildSlots } from '../api/buildSlots'

export const BUILD_SLOTS_POLL_MS = 10_000

let snap: BuildSlots | null = null
let lastPush = 0
let timer: ReturnType<typeof setInterval> | null = null
const listeners = new Set<() => void>()

function publish(next: BuildSlots | null) {
  snap = next
  for (const l of listeners) l()
}

function isSnapshot(v: unknown): v is BuildSlots {
  return typeof v === 'object' && v !== null && Array.isArray((v as BuildSlots).slots)
}

export function receiveBuildSlots(data: unknown) {
  if (!isSnapshot(data)) return
  lastPush = Date.now()
  publish(data)
}

export async function reloadBuildSlots() {
  try {
    const next: unknown = await fetchBuildSlots()
    publish(isSnapshot(next) ? next : null)
  } catch {
    if (snap === null) publish(null)
  }
}

export function useBuildSlots(): BuildSlots | null {
  const [s, setS] = useState(snap)
  useEffect(() => {
    const l = () => setS(snap)
    listeners.add(l)
    if (!snap) void reloadBuildSlots()
    timer ??= setInterval(() => {
      if (Date.now() - lastPush >= BUILD_SLOTS_POLL_MS) void reloadBuildSlots()
    }, BUILD_SLOTS_POLL_MS)
    return () => {
      listeners.delete(l)
      if (listeners.size === 0 && timer) {
        clearInterval(timer)
        timer = null
      }
    }
  }, [])
  return s
}

export function resetBuildSlotsForTest() {
  snap = null
  lastPush = 0
  listeners.clear()
  if (timer) clearInterval(timer)
  timer = null
}
