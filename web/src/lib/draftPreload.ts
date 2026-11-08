import type { Draft } from './choiceDraft'

/**
 * 預載每顆 bot、每份問卷只跑一份，module 層共用：BlockedPanel 與 BlockedModal 同時預載會互相
 * 插隊導覽鍵（docs/reviews/2026-09-12/web.md §1 BlockedDraft），StrictMode 雙 mount 亦同。
 * 引用計數，歸零後延遲一拍才丟（StrictMode cleanup 與第二次 mount 同一 commit）；失敗不留表。
 */

export type Progress = (done: number, total: number) => void
export type StartPreload = (onProgress: Progress, signal: AbortSignal) => Promise<Draft | null>

interface Entry {
  job: Promise<Draft | null>
  /** 沒人持有了（或被換掉）就中止，預載不再對終端送導覽鍵（issue #908）。 */
  controller: AbortController
  refs: number
  listeners: Set<Progress>
  last: { done: number; total: number } | null
  drop: ReturnType<typeof setTimeout> | null
  /** 已經成功讀完（有草稿）。 */
  done: boolean
}

export interface PreloadHandle {
  job: Promise<Draft | null>
  release: () => void
}

const entries = new Map<string, Entry>()
/** 讀完的草稿在最後一位持有者放手後保留多久（毫秒）。 */
const DONE_KEEP_MS = 8000
/** 進行中的預載：最後一位持有者放手後，寬限多久才中止（毫秒）。 */
const IN_FLIGHT_GRACE_MS = 1500

function begin(key: string, start: StartPreload): Entry {
  const controller = new AbortController()
  const entry: Entry = { job: Promise.resolve(null), controller, refs: 0, listeners: new Set(), last: null, drop: null, done: false }
  // 丟例外也是失敗：收成 `null`，持有者與下面的「失敗不留表」才一致（issue #1718）。
  entry.job = start((done, total) => {
    entry.last = { done, total }
    for (const l of entry.listeners) l(done, total)
  }, controller.signal).catch(() => null)
  void entry.job.then((d) => {
    entry.done = d !== null
    if (d === null && entries.get(key) === entry) entries.delete(key)
  })
  entries.set(key, entry)
  return entry
}

function attach(key: string, entry: Entry, onProgress?: Progress): PreloadHandle {
  entry.refs += 1
  if (entry.drop !== null) {
    clearTimeout(entry.drop)
    entry.drop = null
  }
  if (onProgress) {
    entry.listeners.add(onProgress)
    if (entry.last) onProgress(entry.last.done, entry.last.total)
  }
  let released = false
  return {
    job: entry.job,
    release: () => {
      if (released) return
      released = true
      if (onProgress) entry.listeners.delete(onProgress)
      entry.refs -= 1
      if (entry.refs > 0) return
      // 已經讀完的草稿多留一段：視窗或面板重掛（畫面換頁時常見）要接回同一份，不要從已經走動過的終端重讀。
      // 進行中的預載照舊，放手就中止（#908）。
      if (entry.done) {
        entry.drop = setTimeout(() => {
          entry.drop = null
          if (entry.refs === 0 && entries.get(key) === entry) entries.delete(key)
        }, DONE_KEEP_MS)
        return
      }
      // 進行中的預載：寬限一小段再中止。面板先放掉、視窗緊接著掛上來（畫面一打開就是這樣）時要接回同一份，
      // 不然中途停下，終端停在走到一半的分頁，新的一份從那裡重讀就對不上。
      entry.drop = setTimeout(() => {
        entry.drop = null
        if (entry.refs === 0 && entries.get(key) === entry) {
          entries.delete(key)
          entry.controller.abort()
        }
      }, IN_FLIGHT_GRACE_MS)
    },
  }
}

export function acquirePreload(key: string, start: StartPreload, onProgress?: Progress): PreloadHandle {
  const entry = entries.get(key) ?? begin(key, start)
  return attach(key, entry, onProgress)
}

/** 「重新讀取」：換新的一份；舊持有者的 promise 不變。 */
export function restartPreload(key: string, start: StartPreload, onProgress?: Progress): PreloadHandle {
  const old = entries.get(key)
  if (old?.drop !== null && old?.drop !== undefined) {
    clearTimeout(old.drop)
    old.drop = null
  }
  old?.controller.abort()
  const entry = begin(key, start)
  // Handles keep releasing the entry they attached to; the replacement only counts new handles.
  return attach(key, entry, onProgress)
}

/** 測試用。 */
export function resetPreloads() {
  for (const e of entries.values()) {
    if (e.drop !== null) clearTimeout(e.drop)
    e.controller.abort()
  }
  entries.clear()
}

/** 測試用。 */
export function hasPreload(key: string): boolean {
  return entries.has(key)
}
