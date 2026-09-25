import type { TerminalSnapshot } from '../api/types'
import { MIN_TABS, preload, type Io } from './choiceDraft'
import { acquirePreload, type PreloadHandle } from './draftPreload'
import { parseChoiceMenu, type TuiChoiceMenu } from './tuiChoices'

/**
 * bot 一進 `blocked` 就先把問卷讀好（2026-09-25 使用者：「這種輸入不要點擊才載入太慢了，應該要預先載入」）。
 *
 * 以前點開 `BlockedModal` 才開始：先等第一張終端快照、再解析、再預載多分頁問卷，每一步都在使用者眼前跑。
 * 現在由 App 掛一次的 `useBlockedPrefetch` 在背景做兩件事：
 * 1. 讀那顆 bot 的 `visible` 快照放進這裡的快取，`useTerminalSnapshot` 開頭就拿來畫，視窗打開當下就有題目。
 * 2. 多分頁問卷另外用 `BlockedDraft` 同一把 key（`<bot>:<分頁標籤>`）起預載並握著，視窗打開時接上已經跑完的那份。
 *    預載只送導覽鍵、不走進 Submit 頁（`choiceDraft.preload`），單題問卷一顆鍵都不送。
 *
 * 只在這個分頁看得見時預載多分頁問卷：背景分頁與別的裝置同時送導覽鍵會互相插隊。
 */

/** `BlockedModal` 用的快照參數；只有同一組才拿快取。 */
export const PREFETCH_SOURCE = 'visible'
export const PREFETCH_LINES = 200
/** 快取超過這麼久就不拿來畫（畫面可能已經換了）。 */
export const PREFETCH_FRESH_MS = 10_000

interface Cached {
  snap: TerminalSnapshot
  at: number
}

const snaps = new Map<string, Cached>()
const held = new Map<string, { key: string; handle: PreloadHandle }>()

export function peekPrefetched(botId: string, source: string, lines: number, now = Date.now()): TerminalSnapshot | null {
  if (source !== PREFETCH_SOURCE || lines !== PREFETCH_LINES) return null
  const c = snaps.get(botId)
  return c && now - c.at <= PREFETCH_FRESH_MS ? c.snap : null
}

/** 跟 `BlockedDraft` 的 key 一模一樣，否則視窗會另起一份預載。 */
export function draftKey(botId: string, menu: TuiChoiceMenu): string {
  return `${botId}:${menu.tabs.map((t) => t.label).join('')}`
}

export interface PrefetchDeps {
  read: (botId: string) => Promise<TerminalSnapshot>
  io: (botId: string) => Io
  /** 這個分頁現在看得見嗎（多分頁問卷才看）。 */
  visible: () => boolean
  now?: () => number
}

/** 讀一次、存快取，必要時起預載。回傳認到的選單（測試用）。 */
export async function prefetchBlocked(botId: string, deps: PrefetchDeps): Promise<TuiChoiceMenu | null> {
  let snap: TerminalSnapshot
  try {
    snap = await deps.read(botId)
  } catch {
    return null
  }
  snaps.set(botId, { snap, at: (deps.now ?? Date.now)() })
  const menu = parseChoiceMenu(snap.text)
  if (!menu || menu.tabs.length < MIN_TABS || !deps.visible()) return menu
  const key = draftKey(botId, menu)
  if (held.get(botId)?.key === key) return menu
  held.get(botId)?.handle.release()
  const io = deps.io(botId)
  held.set(botId, { key, handle: acquirePreload(key, (progress) => preload(io, menu, progress)) })
  return menu
}

/** bot 不再 blocked：放掉快取與握著的預載。 */
export function forgetBlocked(botId: string): void {
  snaps.delete(botId)
  held.get(botId)?.handle.release()
  held.delete(botId)
}

/** 測試用。 */
export function resetBlockedPrefetch(): void {
  for (const id of [...held.keys()]) forgetBlocked(id)
  snaps.clear()
}
