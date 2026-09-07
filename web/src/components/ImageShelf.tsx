/**
 * The image shelf: a staging area for images that belongs to no conversation.
 *
 * The composer's own tray is per-draft and dies with the panel, so a screenshot you happen
 * to have in front of bot A cannot be walked over to bot B. Park it here instead — the
 * shelf lives outside `<main>` in `App.tsx`, so switching bot / project / team never
 * unmounts it — then hand it to whichever conversation is open, by dragging it into the
 * chat (desktop) or tapping it (everywhere, and the only way on touch).
 *
 * Nothing is uploaded while an image waits here: an attachment id is scoped to the
 * receiving bot's project, so the upload has to happen against the conversation that ends
 * up with the image, which is what the drop / tap does via that panel's `useAttachments`.
 * Design notes in `docs/goals/image-drop-tray-2026-09-07.md`.
 */

import { useEffect, useRef, useState } from 'react'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { ImageIcon, formatSize, useDropTarget } from './Attachments'
import { MAX_BYTES, SHELF_MAX, SHELF_MIME, useShelf } from '../store/shelf'
import type { ShelfItem } from '../store/shelf'
import { useStore } from '../store/store'

/** Collapsed / expanded is a layout preference, so it — and only it — is remembered. */
const OPEN_KEY = 'am.shelf.open'

function readOpen(): boolean {
  try {
    return localStorage.getItem(OPEN_KEY) !== '0'
  } catch {
    return true
  }
}

/**
 * `true` while a drag carrying OS files is anywhere over the window. The collapsed rail is
 * 38px wide — too small to aim at — so it grows a drop pad for the duration of the drag.
 */
function useFileDragActive(): boolean {
  const [active, setActive] = useState(false)
  useEffect(() => {
    let depth = 0
    const hasFiles = (e: DragEvent) => Array.from(e.dataTransfer?.types ?? []).includes('Files')
    const onEnter = (e: DragEvent) => {
      if (!hasFiles(e)) return
      depth += 1
      setActive(true)
    }
    const onLeave = (e: DragEvent) => {
      if (!hasFiles(e)) return
      depth = Math.max(0, depth - 1)
      if (depth === 0) setActive(false)
    }
    const stop = () => {
      depth = 0
      setActive(false)
    }
    window.addEventListener('dragenter', onEnter)
    window.addEventListener('dragleave', onLeave)
    window.addEventListener('drop', stop)
    window.addEventListener('dragend', stop)
    return () => {
      window.removeEventListener('dragenter', onEnter)
      window.removeEventListener('dragleave', onLeave)
      window.removeEventListener('drop', stop)
      window.removeEventListener('dragend', stop)
    }
  }, [])
  return active
}

/**
 * Register the conversation on screen as the shelf's hand-off target: its `useAttachments`
 * `add`, plus a name for the shelf's own tooltip. Called by the chat panels; the shelf has
 * no other way to know which composer is live, and this is what makes the upload land on
 * the bot the user actually meant.
 */
export function useShelfSink(add: (files: File[]) => void, label: string | null) {
  useEffect(() => {
    if (!label) return
    const sink = { add, label }
    useShelf.getState().setSink(sink)
    return () => {
      // Only clear our own registration: on a panel swap the next panel's effect may
      // already have run.
      if (useShelf.getState().sink === sink) useShelf.getState().setSink(null)
    }
  }, [add, label])
}

export function ImageShelf() {
  const items = useShelf((s) => s.items)
  const sink = useShelf((s) => s.sink)
  const addToShelf = useShelf((s) => s.add)
  const clear = useShelf((s) => s.clear)
  const notify = useStore((s) => s.notify)
  const [open, setOpen] = useState(readOpen)
  // 手機沒有拖放，空狀態那段字也會在 390px 折成兩行、白佔掉底部一段：短版只講點得到的那條路。
  const phone = useMediaQuery(PHONE_QUERY)
  const fileDrag = useFileDragActive()
  const picker = useRef<HTMLInputElement>(null)

  const setOpenPersisted = (next: boolean) => {
    setOpen(next)
    try {
      localStorage.setItem(OPEN_KEY, next ? '1' : '0')
    } catch {
      // Private mode / blocked storage: the preference just does not survive a reload.
    }
  }

  /** Park files, and say out loud whatever was refused — a silently dropped image reads as a bug. */
  const take = (files: File[]) => {
    if (files.length === 0) return
    const r = addToShelf(files)
    if (r.notImage > 0) notify('error', `已略過 ${r.notImage} 個非圖片檔案，暫存區只收圖片。`)
    for (const name of r.tooBig) notify('error', `「${name}」超過 ${formatSize(MAX_BYTES)} 上限，沒有放進暫存區。`)
    if (r.overflow > 0) notify('error', `暫存區最多 ${SHELF_MAX} 張，有 ${r.overflow} 張沒放進去。`)
    // Dropping onto the collapsed rail would otherwise look like nothing happened.
    if (r.added > 0 && !open) setOpenPersisted(true)
  }

  const drop = useDropTarget(take)

  const count = items.length

  return (
    <aside
      className={`shelf${open ? ' open' : ''}${drop.over ? ' dropping' : ''}${!open && fileDrag ? ' armed' : ''}`}
      aria-label="圖片暫存區"
      // Works wherever focus is inside the shelf, including on the collapsed handle.
      onPaste={(e) => {
        const imgs = Array.from(e.clipboardData?.files ?? [])
        if (imgs.length === 0) return
        e.preventDefault()
        take(imgs)
      }}
    >
      <input
        ref={picker}
        type="file"
        accept="image/*"
        multiple
        hidden
        onChange={(e) => {
          take(Array.from(e.target.files ?? []))
          // Reset so picking the same file twice still fires `change`.
          e.target.value = ''
        }}
      />
      {open ? (
        <div className="shelf-body" {...drop.props}>
          <div className="shelf-head">
            <ImageIcon />
            <span className="shelf-title">圖片暫存</span>
            {count > 0 ? (
              <span className="shelf-count" aria-label={`暫存 ${count} 張`}>
                {count}
              </span>
            ) : null}
            <button
              type="button"
              className="icon-btn shelf-add icon-tip"
              aria-label="加入圖片到暫存區"
              title={`加入圖片到暫存區（也可以拖進來或貼上）·單檔上限 ${formatSize(MAX_BYTES)}`}
              data-tip="加入圖片 · 暫存"
              onClick={() => picker.current?.click()}
            >
              ＋
            </button>
            {count > 0 ? (
              <button type="button" className="icon-btn shelf-clear icon-tip" aria-label="清空暫存區" title="清空暫存區" data-tip="清空 · 暫存" onClick={clear}>
                ✕
              </button>
            ) : null}
            <button
              type="button"
              className="icon-btn shelf-fold icon-tip"
              aria-expanded={true}
              aria-label="收合圖片暫存區"
              title="收合圖片暫存區"
              data-tip="收合 · 暫存"
              onClick={() => setOpenPersisted(false)}
            >
              »
            </button>
          </div>
          <div className="shelf-list" role="list">
            {count === 0 ? (
              <p className="shelf-empty">
                {phone
                  ? '用 ＋ 把圖片放這裡，之後點一下就進對話。'
                  : '把圖片拖到這裡先放著，換到想給的對話再拖（或點）進去。跨 bot、跨 project 都在，重新整理就清空。'}
              </p>
            ) : (
              items.map((it) => <ShelfCard key={it.key} item={it} sinkLabel={sink?.label ?? null} />)
            )}
          </div>
          {drop.over ? (
            <div className="shelf-veil" aria-hidden="true">
              放開以暫存
            </div>
          ) : null}
        </div>
      ) : (
        <>
          <button
            type="button"
            className="shelf-handle"
            aria-expanded={false}
            aria-label={count > 0 ? `展開圖片暫存區，目前 ${count} 張` : '展開圖片暫存區'}
            title="圖片暫存區：先放著，之後再拖進任何對話"
            onClick={() => setOpenPersisted(true)}
          >
            <ImageIcon />
            {count > 0 ? <span className="shelf-count">{count}</span> : null}
            <span className="shelf-handle-label">圖片暫存</span>
          </button>
          {fileDrag ? (
            <div className={`shelf-pad${drop.over ? ' over' : ''}`} {...drop.props}>
              放到這裡先暫存
            </div>
          ) : null}
        </>
      )}
    </aside>
  )
}

/**
 * One parked image. Click / Enter hands it to the conversation on screen (which is where
 * the upload happens); on desktop it can also be dragged into the chat, carrying just its
 * key — `useDropTarget` fetches the `File` back out of the shelf.
 */
function ShelfCard({ item, sinkLabel }: { item: ShelfItem; sinkLabel: string | null }) {
  const remove = useShelf((s) => s.remove)
  const handed = useShelf((s) => s.handed.includes(item.key))
  const notify = useStore((s) => s.notify)

  const hand = () => {
    const { sink, markHanded } = useShelf.getState()
    if (!sink) {
      notify('error', '先開一個對話（bot 或群組），才能把暫存的圖片放進去。')
      return
    }
    sink.add([item.file])
    markHanded([item.key])
  }

  return (
    <div className={`shelf-card${handed ? ' handed' : ''}`} role="listitem">
      <button
        type="button"
        className="shelf-card-main"
        draggable
        aria-label={`${item.name}，${formatSize(item.size)}${sinkLabel ? `，放進${sinkLabel}` : ''}`}
        title={`${item.name} · ${formatSize(item.size)}\n${sinkLabel ? `點一下放進${sinkLabel}` : '先開一個對話才能放進去'}；Delete 移除`}
        onDragStart={(e) => {
          e.dataTransfer.setData(SHELF_MIME, item.key)
          e.dataTransfer.effectAllowed = 'copy'
        }}
        onClick={hand}
        onKeyDown={(e) => {
          if (e.key !== 'Delete' && e.key !== 'Backspace') return
          e.preventDefault()
          remove(item.key)
        }}
      >
        <img src={item.url} alt="" />
        <span className="shelf-card-name">{item.name}</span>
        <span className="shelf-card-size">{handed ? '已放入' : formatSize(item.size)}</span>
      </button>
      <button type="button" className="shelf-card-x" aria-label={`從暫存區移除 ${item.name}`} title="移除" onClick={() => remove(item.key)}>
        ×
      </button>
    </div>
  )
}
