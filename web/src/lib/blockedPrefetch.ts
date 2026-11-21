import type { TerminalSnapshot } from '../api/types'
import { MIN_TABS, preload, type Io } from './choiceDraft'
import { acquirePreload, type PreloadHandle } from './draftPreload'
import { parseChoiceMenu, type TuiChoiceMenu } from './tuiChoices'

export const PREFETCH_SOURCE = 'visible'
export const PREFETCH_LINES = 200
export const PREFETCH_FRESH_MS = 10_000

interface Cached {
  snap: TerminalSnapshot
  at: number
}

const snaps = new Map<string, Cached>()
const held = new Map<string, { key: string; handle: PreloadHandle }>()
const generation = new Map<string, number>()

export function peekPrefetched(botId: string, source: string, lines: number, now = Date.now()): TerminalSnapshot | null {
  if (source !== PREFETCH_SOURCE || lines !== PREFETCH_LINES) return null
  const c = snaps.get(botId)
  return c && now - c.at <= PREFETCH_FRESH_MS ? c.snap : null
}

export function draftKey(botId: string, menu: TuiChoiceMenu): string {
  return `${botId}:${menu.tabs.map((t) => t.label).join('\x01')}`
}

export interface PrefetchDeps {
  read: (botId: string) => Promise<TerminalSnapshot>
  io: (botId: string) => Io
  stillBlocked?: (botId: string) => boolean
  visible: () => boolean
  now?: () => number
}

export async function prefetchBlocked(botId: string, deps: PrefetchDeps): Promise<TuiChoiceMenu | null> {
  const gen = generation.get(botId) ?? 0
  let snap: TerminalSnapshot
  try {
    snap = await deps.read(botId)
  } catch {
    return null
  }
  if (gen !== (generation.get(botId) ?? 0) || deps.stillBlocked?.(botId) === false) return null
  snaps.set(botId, { snap, at: (deps.now ?? Date.now)() })
  const menu = parseChoiceMenu(snap.text)
  if (!menu || menu.tabs.length < MIN_TABS || !deps.visible()) return menu
  const key = draftKey(botId, menu)
  if (held.get(botId)?.key === key) return menu
  held.get(botId)?.handle.release()
  const io = deps.io(botId)
  held.set(botId, { key, handle: acquirePreload(key, (progress, signal) => preload(io, menu, progress, signal)) })
  return menu
}

export function forgetBlocked(botId: string): void {
  generation.set(botId, (generation.get(botId) ?? 0) + 1)
  snaps.delete(botId)
  held.get(botId)?.handle.release()
  held.delete(botId)
}

export function forgetBlockedExcept(live: ReadonlySet<string>): void {
  for (const id of new Set([...snaps.keys(), ...held.keys()])) if (!live.has(id)) forgetBlocked(id)
}

export function resetBlockedPrefetch(): void {
  for (const id of [...held.keys()]) forgetBlocked(id)
  snaps.clear()
}
