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
 *
 * The cards are 96px wide, which is enough to tell two screenshots apart but not to read
 * one, so hover / focus (and a long press on touch) floats a 300px preview *outside* the
 * shelf — left of the rail on desktop, above the strip on a phone — where it cannot cover
 * the card's own remove button. Design notes in
 * `docs/goals/image-drop-tray-2026-09-07.md`.
 */

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { CSSProperties, PointerEvent as ReactPointerEvent } from 'react'
import { MOBILE_QUERY, PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { ImageIcon, formatSize, useDropTarget } from './Attachments'
import { MAX_BYTES, SHELF_MAX, SHELF_MIME, useShelf } from '../store/shelf'
import type { ShelfItem } from '../store/shelf'
import { useStore } from '../store/store'

/** Collapsed / expanded is a layout preference, so it — and only it — is remembered. */
const OPEN_KEY = 'am.shelf.open'

/** Preview box width; its image is capped at 320px tall (and by the room actually there). */
const PEEK_W = 300
/** A short delay, so sweeping the pointer down the rail does not flash every card. */
const PEEK_HOVER_MS = 180
/** Touch: a long press opens the preview — a plain tap still means 「放進對話」. */
const PEEK_PRESS_MS = 450

/**
 * Where the preview is anchored, in viewport coordinates: the card it belongs to (which it
 * lines up with) and the shelf's own edge (which it stays outside of). Both are needed —
 * clearing just the card would still put the box on top of the header's ＋ / ✕ / » row.
 */
interface PeekAt {
  key: string
  /** Opened by a long press, so the next tap closes it instead of handing the image over. */
  touch: boolean
  top: number
  left: number
  width: number
  height: number
  /** The shelf's top and left; the preview is placed above / left of these. */
  boundTop: number
  boundLeft: number
}

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
  // 同一個斷點決定托盤在右邊還是在底部，也就決定預覽要浮在左邊還是上面。
  const atBottom = useMediaQuery(MOBILE_QUERY)
  const fileDrag = useFileDragActive()
  const picker = useRef<HTMLInputElement>(null)
  const shelfRef = useRef<HTMLElement>(null)
  const [peek, setPeek] = useState<PeekAt | null>(null)
  const peekTimer = useRef<number | null>(null)

  const endPeek = useCallback(() => {
    if (peekTimer.current !== null) {
      window.clearTimeout(peekTimer.current)
      peekTimer.current = null
    }
    setPeek(null)
  }, [])

  /** The card's box is read when the preview actually opens, not when the hover began. */
  const startPeek = useCallback((key: string, el: HTMLElement, delay: number, touch: boolean) => {
    if (peekTimer.current !== null) window.clearTimeout(peekTimer.current)
    const show = () => {
      peekTimer.current = null
      const r = el.getBoundingClientRect()
      const b = shelfRef.current?.getBoundingClientRect()
      setPeek({
        key,
        touch,
        top: r.top,
        left: r.left,
        width: r.width,
        height: r.height,
        boundTop: b?.top ?? r.top,
        boundLeft: b?.left ?? r.left,
      })
    }
    if (delay <= 0) {
      peekTimer.current = null
      show()
    } else {
      peekTimer.current = window.setTimeout(show, delay)
    }
  }, [])

  useEffect(() => {
    return () => {
      if (peekTimer.current !== null) window.clearTimeout(peekTimer.current)
    }
  }, [])

  // The preview is anchored to a box that scrolling / resizing moves, and Escape should
  // dismiss it like any other transient overlay.
  useEffect(() => {
    if (!peek) return
    const off = () => setPeek(null)
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || e.defaultPrevented) return
      // 吃掉這一次 Escape，不然手機上會連著把側欄抽屜一起關掉。
      e.preventDefault()
      setPeek(null)
    }
    // capture: 托盤自己那條捲動不會冒泡到 window。
    window.addEventListener('scroll', off, true)
    window.addEventListener('resize', off)
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('scroll', off, true)
      window.removeEventListener('resize', off)
      window.removeEventListener('keydown', onKey)
    }
  }, [peek])

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
  // The parked image may have been removed (or everything cleared) while its preview was up.
  const peekItem = peek ? (items.find((it) => it.key === peek.key) ?? null) : null

  return (
    <aside
      ref={shelfRef}
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
                  : '把圖片拖到這裡先放著，換到想給的對話再拖（或點）進去。跨 bot、跨 project 都在，重新整理也還在，放進來 30 分鐘後自動清掉。'}
              </p>
            ) : (
              items.map((it) => (
                <ShelfCard
                  key={it.key}
                  item={it}
                  sinkLabel={sink?.label ?? null}
                  peek={peek?.key === it.key ? (peek.touch ? 'touch' : 'pointer') : 'off'}
                  onPeekStart={startPeek}
                  onPeekEnd={endPeek}
                />
              ))
            )}
          </div>
          {drop.over ? (
            <div className="shelf-veil" aria-hidden="true">
              放開以暫存
            </div>
          ) : null}
          {peekItem && peek ? <ShelfPeek item={peekItem} at={peek} above={atBottom} /> : null}
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
 *
 * Hover and focus open the big preview. On touch that is a long press instead: a tap is
 * already the hand-over, and taking it away would leave phones with no way to use the
 * shelf at all. The press swallows the click it produces, and the next tap closes.
 */
function ShelfCard({
  item,
  sinkLabel,
  peek,
  onPeekStart,
  onPeekEnd,
}: {
  item: ShelfItem
  sinkLabel: string | null
  /** Whether this card's preview is up, and what opened it. */
  peek: 'off' | 'pointer' | 'touch'
  onPeekStart: (key: string, el: HTMLElement, delay: number, touch: boolean) => void
  onPeekEnd: () => void
}) {
  const remove = useShelf((s) => s.remove)
  const handed = useShelf((s) => s.handed.includes(item.key))
  const notify = useStore((s) => s.notify)
  const press = useRef<{ timer: number; x: number; y: number } | null>(null)
  const swallowClick = useRef(false)

  const hand = () => {
    const { sink, markHanded } = useShelf.getState()
    if (!sink) {
      notify('error', '先開一個對話（bot 或群組），才能把暫存的圖片放進去。')
      return
    }
    sink.add([item.file])
    markHanded([item.key])
  }

  const endPress = () => {
    if (press.current) window.clearTimeout(press.current.timer)
    press.current = null
  }

  const onPointerDown = (e: ReactPointerEvent<HTMLButtonElement>) => {
    if (e.pointerType === 'mouse') return
    const el = e.currentTarget
    endPress()
    press.current = {
      x: e.clientX,
      y: e.clientY,
      timer: window.setTimeout(() => {
        press.current = null
        // The press is over as far as we are concerned; the click it still fires is not ours.
        swallowClick.current = true
        onPeekStart(item.key, el, 0, true)
      }, PEEK_PRESS_MS),
    }
  }

  return (
    <div className={`shelf-card${handed ? ' handed' : ''}${peek === 'off' ? '' : ' peeking'}`} role="listitem">
      <button
        type="button"
        className="shelf-card-main"
        draggable
        aria-label={`${item.name}，${formatSize(item.size)}${sinkLabel ? `，放進${sinkLabel}` : ''}`}
        title={`${item.name} · ${formatSize(item.size)}\n${sinkLabel ? `點一下放進${sinkLabel}` : '先開一個對話才能放進去'}；Delete 移除`}
        onDragStart={(e) => {
          e.dataTransfer.setData(SHELF_MIME, item.key)
          e.dataTransfer.effectAllowed = 'copy'
          // 拖著的時候不要再浮一張大圖在旁邊。
          onPeekEnd()
        }}
        onPointerEnter={(e) => {
          if (e.pointerType === 'mouse') onPeekStart(item.key, e.currentTarget, PEEK_HOVER_MS, false)
        }}
        onPointerLeave={(e) => {
          if (e.pointerType === 'mouse') onPeekEnd()
          endPress()
        }}
        onPointerDown={onPointerDown}
        onPointerUp={endPress}
        onPointerCancel={endPress}
        onPointerMove={(e) => {
          // 捲動或滑開就不算長按。
          if (!press.current) return
          if (Math.abs(e.clientX - press.current.x) > 10 || Math.abs(e.clientY - press.current.y) > 10) endPress()
        }}
        onFocus={(e) => onPeekStart(item.key, e.currentTarget, 0, false)}
        onBlur={onPeekEnd}
        onClick={() => {
          if (swallowClick.current) {
            swallowClick.current = false
            return
          }
          // 長按開著預覽時，再點一下是收起來，不是放進對話。
          if (peek === 'touch') {
            onPeekEnd()
            return
          }
          hand()
        }}
        onKeyDown={(e) => {
          if (e.key !== 'Delete' && e.key !== 'Backspace') return
          e.preventDefault()
          onPeekEnd()
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

/**
 * The floating preview. Fixed-positioned outside the shelf — left of the rail, or above
 * the strip when the shelf is along the bottom — so it never covers the card it belongs to
 * (and so its × stays clickable); `pointer-events: none` makes that impossible anyway.
 *
 * The box is placed after measuring it, in a layout effect, so it lands on screen in one
 * paint. Its height is capped by the room actually available above the card
 * (`--peek-avail`), which is what keeps the "above" placement from being clamped back down
 * on top of the strip on a short landscape phone.
 */
function ShelfPeek({ item, at, above }: { item: ShelfItem; at: PeekAt; above: boolean }) {
  const box = useRef<HTMLDivElement>(null)
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)
  const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v))

  const place = useCallback(() => {
    const el = box.current
    if (!el) return
    const w = el.offsetWidth
    const h = el.offsetHeight
    if (above) {
      // Above the whole shelf (header included), lined up with the card.
      setPos({
        left: clamp(at.left + at.width / 2 - w / 2, 8, Math.max(8, window.innerWidth - w - 8)),
        top: clamp(at.boundTop - 10 - h, 8, Math.max(8, at.boundTop - 10 - h)),
      })
    } else {
      // Left of the rail, vertically centred on the card.
      setPos({
        left: clamp(at.boundLeft - 10 - w, 8, Math.max(8, at.boundLeft - 10 - w)),
        top: clamp(at.top + at.height / 2 - h / 2, 8, Math.max(8, window.innerHeight - h - 8)),
      })
    }
  }, [at, above])

  useLayoutEffect(place, [place])

  // 上方（或左側）真正剩下的空間；圖片跟著縮，而不是被夾回托盤身上。
  const avail = above ? Math.max(96, at.boundTop - 18) : window.innerHeight - 16
  const style: CSSProperties = {
    '--peek-avail': `${avail}px`,
    width: `min(${PEEK_W}px, calc(100vw - 24px))`,
    // 量完之前先放在畫面外：layout effect 會在同一次繪製前補上真正的位置。
    left: pos ? pos.left : -9999,
    top: pos ? pos.top : 0,
  } as CSSProperties

  return (
    <div ref={box} className="shelf-peek" style={style} aria-hidden="true">
      <img src={item.url} alt="" onLoad={place} />
      <span className="shelf-peek-cap">
        {item.name} · {formatSize(item.size)}
      </span>
    </div>
  )
}
