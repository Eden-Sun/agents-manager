/**
 * The image shelf: a cross-conversation staging area for images.
 *
 * You often have a screenshot in front of bot A but want it read by bot B. The composer's
 * own tray cannot help — it belongs to one draft and is thrown away when the panel
 * remounts on a bot switch. So images can be parked here first, and handed to whichever
 * conversation is open later.
 *
 * Nothing is uploaded while an image sits here: attachments are scoped to the receiving
 * bot's project (`attach::resolve`), so the id from one bot is useless to another. The
 * shelf therefore holds the raw `File`, and the upload happens at the moment the image is
 * dropped (or tapped) into a conversation, through the existing `useAttachments`.
 *
 * A store of its own rather than a slice of `useStore`: that one mirrors parts of itself to
 * localStorage and has `refreshState` replace whole slices from the daemon, neither of
 * which a `File` or an object URL survives. Reloading the page empties the shelf, which is
 * what the feature asks for.
 */

import { create } from 'zustand'

/**
 * The per-file ceiling, checked here so the error is instant. Same number as
 * `attach::MAX_BYTES` on the daemon side, and shared with the composer's own tray
 * (`Attachments.tsx` imports it from here) so the shelf can never park a file the
 * conversation would then refuse.
 */
export const MAX_BYTES = 12 * 1024 * 1024

/**
 * How many images may be parked at once. Neither the daemon nor `useAttachments` caps the
 * count, but both only live until a send; the shelf lives until a reload, so it needs a
 * ceiling of its own — 24 × 12MB is the worst case held in memory.
 */
export const SHELF_MAX = 24

/** The drag payload for an internal shelf → composer drag; the value is the item key. */
export const SHELF_MIME = 'application/x-am-shelf'

export interface ShelfItem {
  key: string
  file: File
  name: string
  size: number
  /** `createObjectURL(file)`, revoked when the item leaves the shelf. */
  url: string
}

/**
 * Where a shelf image goes when it is tapped: the attachment tray of the conversation that
 * is currently on screen. Registered by whichever chat panel is mounted (`ChatPanel`,
 * `GroupChatPanel`); `null` while the open panel has no composer (a team, or no bot yet).
 */
export interface ShelfSink {
  add: (files: File[]) => void
  /** Shown on the shelf's own button: 「放進 <label>」. */
  label: string
}

interface ShelfState {
  items: ShelfItem[]
  sink: ShelfSink | null
  /** Item keys that were just handed to a conversation, for the brief 「已放入」 flash. */
  handed: string[]
  add: (files: File[]) => { added: number; tooBig: string[]; notImage: number; overflow: number }
  remove: (key: string) => void
  clear: () => void
  /** The parked `File`s for these keys, in shelf order; unknown keys are skipped. */
  filesFor: (keys: string[]) => File[]
  markHanded: (keys: string[]) => void
  setSink: (sink: ShelfSink | null) => void
}

let seq = 0

export const useShelf = create<ShelfState>((set, get) => ({
  items: [],
  sink: null,
  handed: [],

  add: (files) => {
    const result = { added: 0, tooBig: [] as string[], notImage: 0, overflow: 0 }
    const accepted: ShelfItem[] = []
    let room = SHELF_MAX - get().items.length
    for (const file of files) {
      if (!file.type.startsWith('image/')) {
        result.notImage += 1
        continue
      }
      if (file.size > MAX_BYTES) {
        result.tooBig.push(file.name || '圖片')
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
        name: file.name || '圖片',
        size: file.size,
        url: URL.createObjectURL(file),
      })
    }
    result.added = accepted.length
    if (accepted.length) set((s) => ({ items: [...s.items, ...accepted] }))
    return result
  },

  remove: (key) =>
    set((s) => {
      const gone = s.items.find((it) => it.key === key)
      if (gone) URL.revokeObjectURL(gone.url)
      return { items: s.items.filter((it) => it.key !== key), handed: s.handed.filter((k) => k !== key) }
    }),

  clear: () =>
    set((s) => {
      for (const it of s.items) URL.revokeObjectURL(it.url)
      return { items: [], handed: [] }
    }),

  filesFor: (keys) => get().items.filter((it) => keys.includes(it.key)).map((it) => it.file),

  markHanded: (keys) => {
    set((s) => ({ handed: [...new Set([...s.handed, ...keys])] }))
    setTimeout(() => set((s) => ({ handed: s.handed.filter((k) => !keys.includes(k)) })), 1400)
  },

  setSink: (sink) => set({ sink }),
}))

/**
 * Read the shelf outside React — `useDropTarget` resolves an internal drag's keys back to
 * `File`s from inside a DOM event handler.
 */
export function shelfFilesFor(keys: string[]): File[] {
  const files = useShelf.getState().filesFor(keys)
  if (files.length) useShelf.getState().markHanded(keys)
  return files
}
