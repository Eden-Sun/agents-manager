import type { Draft } from './choiceDraft'

/**
 * 預載每顆 bot、每份問卷只跑一份，module 層共用：BlockedPanel 與 BlockedModal 同時預載會互相
 * 插隊導覽鍵（docs/reviews/2026-09-12/web.md §1 BlockedDraft），StrictMode 雙 mount 亦同。
 * 引用計數，歸零後延遲一拍才丟（StrictMode cleanup 與第二次 mount 同一 commit）；失敗不留表。
 */

export type Progress = (done: number, total: number) => void
export type StartPreload = (onProgress: Progress) => Promise<Draft | null>

interface Entry {
  job: Promise<Draft | null>
  refs: number
  listeners: Set<Progress>
  last: { done: number; total: number } | null
  drop: ReturnType<typeof setTimeout> | null
}

export interface PreloadHandle {
  job: Promise<Draft | null>
  release: () => void
}

const entries = new Map<string, Entry>()

function begin(key: string, start: StartPreload): Entry {
  const entry: Entry = { job: Promise.resolve(null), refs: 0, listeners: new Set(), last: null, drop: null }
  entry.job = start((done, total) => {
    entry.last = { done, total }
    for (const l of entry.listeners) l(done, total)
  })
  void entry.job.then((d) => {
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
      entry.drop = setTimeout(() => {
        entry.drop = null
        if (entry.refs === 0 && entries.get(key) === entry) entries.delete(key)
      }, 0)
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
  if (old?.drop !== null && old?.drop !== undefined) clearTimeout(old.drop)
  const entry = begin(key, start)
  // 搬計數：舊持有者放掉時不該把新的丟掉。
  if (old) entry.refs = old.refs
  return attach(key, entry, onProgress)
}

/** 測試用。 */
export function resetPreloads() {
  for (const e of entries.values()) if (e.drop !== null) clearTimeout(e.drop)
  entries.clear()
}

/** 測試用。 */
export function hasPreload(key: string): boolean {
  return entries.has(key)
}
