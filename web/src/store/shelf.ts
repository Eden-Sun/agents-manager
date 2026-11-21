
import { create } from 'zustand'
import { t } from '../lib/i18n'

export const MAX_BYTES = 50 * 1024 * 1024

export const SHELF_MAX = 24

export const SHELF_TTL_MS = 30 * 60 * 1000

export const SHELF_MIME = 'application/x-am-shelf'

export interface ShelfItem {
  key: string
  file: File
  name: string
  size: number
  isImage: boolean
  url: string
  addedAt: number
}

export interface ShelfSink {
  add: (files: File[]) => void
  label: string
}

interface ShelfState {
  items: ShelfItem[]
  sink: ShelfSink | null
  handed: string[]
  add: (files: File[]) => { added: number; tooBig: string[]; overflow: number }
  remove: (key: string) => void
  clear: () => void
  filesFor: (keys: string[]) => File[]
  markHanded: (keys: string[]) => void
  setSink: (sink: ShelfSink | null) => void
  restore: (items: { key: string; file: File; addedAt: number }[]) => void
  expire: () => string[]
}

function isImage(file: File): boolean {
  return file.type.startsWith('image/')
}

let seq = 0
const TAB = Math.random().toString(36).slice(2, 8)

const initialShelfState = import.meta.hot?.data?.shelfState

export const useShelf = create<ShelfState>((set, get) => ({
  items: initialShelfState?.items ?? [],
  sink: initialShelfState?.sink ?? null,
  handed: initialShelfState?.handed ?? [],

  add: (files) => {
    const result = { added: 0, tooBig: [] as string[], overflow: 0 }
    const accepted: ShelfItem[] = []
    let room = SHELF_MAX - get().items.length
    for (const file of files) {
      if (file.size > MAX_BYTES) {
        result.tooBig.push(file.name || t('檔案'))
        continue
      }
      if (room <= 0) {
        result.overflow += 1
        continue
      }
      room -= 1
      seq += 1
      accepted.push({
        key: `s${TAB}${seq}`,
        file,
        name: file.name || t('檔案'),
        size: file.size,
        isImage: isImage(file),
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
        const legacy = /^s(\d+)$/.exec(it.key)
        const n = legacy ? Number(legacy[1]) : NaN
        if (Number.isSafeInteger(n) && n > seq) seq = n
        back.push({
          key: it.key,
          file: it.file,
          name: it.file.name || t('檔案'),
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

export function shelfFilesFor(keys: string[]): File[] {
  const files = useShelf.getState().filesFor(keys)
  if (files.length) useShelf.getState().markHanded(keys)
  return files
}

if (import.meta.hot) {
  import.meta.hot.dispose((data: Record<string, unknown>) => {
    data.shelfState = useShelf.getState()
  })
}
