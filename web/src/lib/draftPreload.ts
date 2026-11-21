import type { Draft } from './choiceDraft'

export type Progress = (done: number, total: number) => void
export type StartPreload = (onProgress: Progress, signal: AbortSignal) => Promise<Draft | null>

interface Entry {
  job: Promise<Draft | null>
  controller: AbortController
  refs: number
  listeners: Set<Progress>
  last: { done: number; total: number } | null
  drop: ReturnType<typeof setTimeout> | null
  done: boolean
}

export interface PreloadHandle {
  job: Promise<Draft | null>
  release: () => void
}

const entries = new Map<string, Entry>()
const DONE_KEEP_MS = 8000
const IN_FLIGHT_GRACE_MS = 1500

function begin(key: string, start: StartPreload): Entry {
  const controller = new AbortController()
  const entry: Entry = { job: Promise.resolve(null), controller, refs: 0, listeners: new Set(), last: null, drop: null, done: false }
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
      if (entry.done) {
        entry.drop = setTimeout(() => {
          entry.drop = null
          if (entry.refs === 0 && entries.get(key) === entry) entries.delete(key)
        }, DONE_KEEP_MS)
        return
      }
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

export function restartPreload(key: string, start: StartPreload, onProgress?: Progress): PreloadHandle {
  const old = entries.get(key)
  if (old?.drop !== null && old?.drop !== undefined) {
    clearTimeout(old.drop)
    old.drop = null
  }
  old?.controller.abort()
  const entry = begin(key, start)
  return attach(key, entry, onProgress)
}

export function resetPreloads() {
  for (const e of entries.values()) {
    if (e.drop !== null) clearTimeout(e.drop)
    e.controller.abort()
  }
  entries.clear()
}

export function hasPreload(key: string): boolean {
  return entries.has(key)
}
