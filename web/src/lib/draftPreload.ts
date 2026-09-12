import type { Draft } from './choiceDraft'

/**
 * 多分頁問卷的預載（`choiceDraft.preload`）**每顆 bot、每份問卷只跑一份**，掛在 module 層讓
 * 幾個視圖共用。
 *
 * 為什麼：對話上方的 `BlockedPanel` 與 1 秒後自動彈出的 `BlockedModal` 會同時掛著各自的
 * `BlockedDraft`，兩份預載同時對同一個 pane 送 ←／→／tab 導覽鍵互相插隊，走到一半畫面對不上
 * 就整份退回即時模式，或把終端留在別的分頁（docs/reviews/2026-09-12/web.md §1 BlockedDraft）。
 * StrictMode 在 dev 的 mount → cleanup → mount 也是同一個問題，以前靠元件裡的 ref 擋，現在
 * 一併由這裡處理。
 *
 * 生命週期用引用計數：最後一個視圖放掉之後**延遲一拍**才丟（StrictMode 的 cleanup 與第二次
 * mount 在同一個 commit 裡，馬上丟會重跑）。預載失敗（`null`）不留在表裡，下次掛上來會重試。
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

/** 接上這個 key 的預載：已經有人在跑就共用那一份，否則現在開始。 */
export function acquirePreload(key: string, start: StartPreload, onProgress?: Progress): PreloadHandle {
  const entry = entries.get(key) ?? begin(key, start)
  return attach(key, entry, onProgress)
}

/**
 * 使用者按「重新讀取」：不管現有的那份，換一份新的。舊的持有者手上的 promise 不變（它們的
 * 結果已經拿到了），之後 acquire 的人都接新的。
 */
export function restartPreload(key: string, start: StartPreload, onProgress?: Progress): PreloadHandle {
  const old = entries.get(key)
  if (old?.drop !== null && old?.drop !== undefined) clearTimeout(old.drop)
  const entry = begin(key, start)
  // 舊持有者放掉時不該把新的一份丟掉：把它們的計數搬過來。
  if (old) entry.refs = old.refs
  return attach(key, entry, onProgress)
}

/** 測試用。 */
export function resetPreloads() {
  for (const e of entries.values()) if (e.drop !== null) clearTimeout(e.drop)
  entries.clear()
}

/** 測試用：這個 key 現在有沒有一份在表裡。 */
export function hasPreload(key: string): boolean {
  return entries.has(key)
}
