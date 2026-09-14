/**
 * Image shelf: park images across conversations (the composer tray dies on bot switch).
 * Holds raw `File`s — attachments are project-scoped (`attach::resolve`), so upload happens on
 * drop into a conversation. Own store, not `useStore`: its localStorage mirror and `refreshState`
 * slice replacement can't carry a `File`/object URL. Persisted by `shelfPersist.ts`.
 */

import { create } from 'zustand'

/** Must equal daemon `attach::MAX_BYTES`; shared with `Attachments.tsx`. */
export const MAX_BYTES = 12 * 1024 * 1024

/** The shelf outlives a send, so it needs its own cap (24 × 12MB worst case in memory). */
export const SHELF_MAX = 24

/** Counted from when parked (SPEC: 30 分鐘). */
export const SHELF_TTL_MS = 30 * 60 * 1000

/** Internal shelf → composer drag payload; value is the item key. */
export const SHELF_MIME = 'application/x-am-shelf'

export interface ShelfItem {
  key: string
  file: File
  name: string
  size: number
  /** Only an image can be previewed; everything else is drawn as an icon card. */
  isImage: boolean
  /** Object URL for an image, revoked when the item leaves the shelf; `''` otherwise. */
  url: string
  addedAt: number
}

/** Tap target: the on-screen conversation's tray, registered by the mounted chat panel. */
export interface ShelfSink {
  add: (files: File[]) => void
  /** 「放進 <label>」 */
  label: string
}

interface ShelfState {
  items: ShelfItem[]
  sink: ShelfSink | null
  /** Keys just handed off, for the brief 「已放入」 flash. */
  handed: string[]
  add: (files: File[]) => { added: number; tooBig: string[]; overflow: number }
  remove: (key: string) => void
  clear: () => void
  filesFor: (keys: string[]) => File[]
  markHanded: (keys: string[]) => void
  setSink: (sink: ShelfSink | null) => void
  /** From IndexedDB on load; keeps original `addedAt`. */
  restore: (items: { key: string; file: File; addedAt: number }[]) => void
  /** Returns the keys removed. */
  expire: () => string[]
}

/** A `File` restored from IndexedDB keeps its `type`, so this works on both paths. */
function isImage(file: File): boolean {
  return file.type.startsWith('image/')
}

let seq = 0

export const useShelf = create<ShelfState>((set, get) => ({
  items: [],
  sink: null,
  handed: [],

  add: (files) => {
    const result = { added: 0, tooBig: [] as string[], overflow: 0 }
    const accepted: ShelfItem[] = []
    let room = SHELF_MAX - get().items.length
    for (const file of files) {
      if (file.size > MAX_BYTES) {
        result.tooBig.push(file.name || '檔案')
        continue
      }
      if (room <= 0) {
        result.overflow += 1
        continue
      }
      room -= 1
      seq += 1
      accepted.push({
        key: `s${seq}`,
        file,
        name: file.name || '檔案',
        size: file.size,
        isImage: isImage(file),
        // 只有圖片需要 object URL；其他檔案畫的是圖示，開一條 blob 只是白佔記憶體。
        url: isImage(file) ? URL.createObjectURL(file) : '',
        addedAt: Date.now(),
      })
    }
    result.added = accepted.length
    if (accepted.length) set((s) => ({ items: [...s.items, ...accepted] }))
    return result
  },

  remove: (key) =>
    set((s) => {
      const gone = s.items.find((it) => it.key === key)
      if (gone?.url) URL.revokeObjectURL(gone.url)
      return { items: s.items.filter((it) => it.key !== key), handed: s.handed.filter((k) => k !== key) }
    }),

  clear: () =>
    set((s) => {
      for (const it of s.items) if (it.url) URL.revokeObjectURL(it.url)
      return { items: [], handed: [] }
    }),

  filesFor: (keys) => get().items.filter((it) => keys.includes(it.key)).map((it) => it.file),

  markHanded: (keys) => {
    set((s) => ({ handed: [...new Set([...s.handed, ...keys])] }))
    setTimeout(() => set((s) => ({ handed: s.handed.filter((k) => !keys.includes(k)) })), 1400)
  },

  setSink: (sink) => set({ sink }),

  restore: (items) =>
    set((s) => {
      const have = new Set(s.items.map((it) => it.key))
      const back: ShelfItem[] = []
      for (const it of items) {
        if (have.has(it.key) || s.items.length + back.length >= SHELF_MAX) continue
        const n = Number(it.key.replace(/^s/, ''))
        if (Number.isFinite(n) && n > seq) seq = n
        back.push({
          key: it.key,
          file: it.file,
          name: it.file.name || '檔案',
          size: it.file.size,
          isImage: isImage(it.file),
          url: isImage(it.file) ? URL.createObjectURL(it.file) : '',
          addedAt: it.addedAt,
        })
      }
      if (!back.length) return {}
      return { items: [...s.items, ...back].sort((a, b) => a.addedAt - b.addedAt) }
    }),

  expire: () => {
    const cutoff = Date.now() - SHELF_TTL_MS
    const gone = get().items.filter((it) => it.addedAt < cutoff)
    if (!gone.length) return []
    for (const it of gone) if (it.url) URL.revokeObjectURL(it.url)
    const keys = gone.map((it) => it.key)
    set((s) => ({ items: s.items.filter((it) => !keys.includes(it.key)), handed: s.handed.filter((k) => !keys.includes(k)) }))
    return keys
  },
}))

/** For `useDropTarget`'s DOM handler (outside React). */
export function shelfFilesFor(keys: string[]): File[] {
  const files = useShelf.getState().filesFor(keys)
  if (files.length) useShelf.getState().markHanded(keys)
  return files
}
