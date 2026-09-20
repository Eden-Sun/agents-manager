/**
 * Image shelf: conversation-independent staging (lives outside `<main>`, survives bot switches).
 * Nothing uploads here — attachment ids are project-scoped, so the receiving panel uploads on hand-off.
 * See `docs/goals/image-drop-tray-2026-09-07.md`.
 */

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { CSSProperties, PointerEvent as ReactPointerEvent } from 'react'
import { DRAWER_QUERY, PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { FileIcon } from './Attachments'
import { formatSize, useDropTarget } from './attachmentsHelpers'
import { MAX_BYTES, SHELF_MAX, SHELF_MIME, useShelf } from '../store/shelf'
import type { ShelfItem } from '../store/shelf'
import { useStore } from '../store/store'
import { OutboxFiles } from './OutboxFiles'
import { MobilePreviewButton } from './MobilePreview'
import './imageShelf.css'

const OPEN_KEY = 'am.shelf.open'

const PEEK_W = 300

/** 預覽撐到對話欄（`.chat`）寬，量不到退回 PEEK_W。 */
function peekWidth(): number {
  const w = document.querySelector('.chat')?.getBoundingClientRect().width ?? 0
  return w > PEEK_W ? Math.round(w - 24) : PEEK_W
}
/** Delay so sweeping down the rail does not flash every card. */
const PEEK_HOVER_MS = 180
/** Touch: a long press opens the preview — a plain tap still means 「放進對話」. */
const PEEK_PRESS_MS = 450

/** Preview anchor: the card (to align) and the shelf edge (to stay outside, else it covers the header buttons). */
interface PeekAt {
  key: string
  /** Opened by a long press, so the next tap closes it instead of handing the image over. */
  touch: boolean
  top: number
  left: number
  width: number
  height: number
  boundTop: number
  boundLeft: number
}

function readOpen(fallback: boolean): boolean {
  try {
    const v = localStorage.getItem(OPEN_KEY)
    return v === null ? fallback : v !== '0'
  } catch {
    return fallback
  }
}

/** `true` while a text field has focus (phone keyboard up) — hides the collapsed bar only. */
function useTypingAway(): boolean {
  const [typing, setTyping] = useState(false)
  useEffect(() => {
    const isField = (el: EventTarget | null) =>
      el instanceof HTMLTextAreaElement || (el instanceof HTMLInputElement && !['checkbox', 'radio', 'file'].includes(el.type))

    // bar 在按下與放開之間出現會把送出鍵推走、click 落空（2026-09-08 390px 實測），
    // 所以延到下一個 task（microtask 會插在 mousedown/mouseup 之間）且等指標放開才改狀態。
    let pointerDown = false
    let pending = false
    const settle = () => {
      if (pointerDown) {
        pending = true
        return
      }
      pending = false
      setTyping(isField(document.activeElement))
    }
    const schedule = () => setTimeout(settle, 0)
    const down = () => {
      pointerDown = true
    }
    const up = () => {
      pointerDown = false
      if (pending) schedule()
    }
    document.addEventListener('focusin', schedule)
    document.addEventListener('focusout', schedule)
    document.addEventListener('pointerdown', down, true)
    document.addEventListener('pointerup', up, true)
    document.addEventListener('pointercancel', up, true)
    return () => {
      document.removeEventListener('focusin', schedule)
      document.removeEventListener('focusout', schedule)
      document.removeEventListener('pointerdown', down, true)
      document.removeEventListener('pointerup', up, true)
      document.removeEventListener('pointercancel', up, true)
    }
  }, [])
  return typing
}

/** `true` while an OS file drag is over the window; the 38px collapsed rail grows a drop pad. */
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

export function ImageShelf() {
  const items = useShelf((s) => s.items)
  const sink = useShelf((s) => s.sink)
  const addToShelf = useShelf((s) => s.add)
  const clear = useShelf((s) => s.clear)
  const notify = useStore((s) => s.notify)
  // 手機沒有拖放，空狀態那段字也會在 390px 折成兩行、白佔掉底部一段：短版只講點得到的那條路。
  const phone = useMediaQuery(PHONE_QUERY)
  // 手機預設收起：展開在 390px 會吃掉約 120px 對話空間。
  const [open, setOpen] = useState(() => readOpen(!window.matchMedia(PHONE_QUERY).matches))
  const typing = useTypingAway()
  const atBottom = useMediaQuery(DRAWER_QUERY)
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
      // Blocked storage: preference just does not survive a reload.
    }
  }

  /** Report whatever was refused — a silently dropped image reads as a bug. */
  const take = (files: File[]) => {
    if (files.length === 0) return
    const r = addToShelf(files)
    for (const name of r.tooBig) notify('error', `「${name}」超過 ${formatSize(MAX_BYTES)} 上限，沒有放進暫存區。`)
    if (r.overflow > 0) notify('error', `暫存區最多 ${SHELF_MAX} 張，有 ${r.overflow} 張沒放進去。`)
    // Dropping onto the collapsed rail would otherwise look like nothing happened.
    if (r.added > 0 && !open) setOpenPersisted(true)
  }

  const drop = useDropTarget(take)

  const count = items.length
  const peekItem = peek ? (items.find((it) => it.key === peek.key) ?? null) : null

  return (
    <aside
      ref={shelfRef}
      className={`shelf${open ? ' open' : ''}${drop.over ? ' dropping' : ''}${!open && fileDrag ? ' armed' : ''}${
        !open && typing ? ' typing' : ''
      }`}
      aria-label="檔案暫存區"
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
        multiple
        hidden
        onChange={(e) => {
          take(Array.from(e.target.files ?? []))
          e.target.value = ''
        }}
      />
      {open && !atBottom ? (
        // 收合鍵做在右欄左邊那條分割線上（2026-09-14 使用者：「收合做在分割線上以節省空間」）：
        // 一顆壓在線上的小耳朵，標題列就不必再擠一顆 »。
        <button
          type="button"
          className="shelf-edge-fold"
          aria-expanded={true}
          aria-label="收合檔案暫存區"
          title="收合檔案暫存區"
          onClick={() => setOpenPersisted(false)}
        >
          »
        </button>
      ) : null}
      {open ? (
        <div className="shelf-body" {...drop.props}>
          <div className="shelf-head">
            <FileIcon />
            <span className="shelf-title">檔案暫存</span>
            {count > 0 ? (
              <span className="shelf-count" aria-label={`暫存 ${count} 個`}>
                {count}
              </span>
            ) : null}
            <button
              type="button"
              className="icon-btn shelf-add icon-tip"
              aria-label="加入檔案到暫存區"
              data-tip="加入檔案 · 暫存"
              onClick={() => picker.current?.click()}
            >
              ＋
            </button>
            {count > 0 ? (
              <button type="button" className="icon-btn shelf-clear icon-tip" aria-label="清空暫存區" data-tip="清空 · 暫存" onClick={clear}>
                ✕
              </button>
            ) : null}
            <MobilePreviewButton />
            {/* 桌機的收合鍵掛在分割線上（見下面的 `.shelf-edge-fold`），標題列省一格；底部那條版面沒有
                左分割線，照舊放在標題列。 */}
            {atBottom ? (
              <button
                type="button"
                className="icon-btn shelf-fold icon-tip"
                aria-expanded={true}
                aria-label="收合檔案暫存區"
                data-tip="收合 · 暫存"
                onClick={() => setOpenPersisted(false)}
              >
                »
              </button>
            ) : null}
          </div>
          <div className="shelf-list" role="list">
            {count === 0 ? (
              <p className="shelf-empty">
                {phone
                  ? '用 ＋ 把檔案放這裡，之後點一下就進對話。'
                  : '把檔案（圖片、PDF、log 都可以）拖到這裡先放著，換到想給的對話再拖（或點）進去。跨 bot、跨 project 都在，重新整理也還在，放進來 30 分鐘後自動清掉。'}
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
          {/* 下半段是**從** bot 拿出來的檔案（唯讀、點了就下載）；上半段是要送進對話的暫存。 */}
          <OutboxFiles />
          {drop.over ? (
            <div className="shelf-veil" aria-hidden="true">
              放開以暫存
            </div>
          ) : null}
          {peekItem?.isImage && peek ? <ShelfPeek item={peekItem} at={peek} above={atBottom} /> : null}
        </div>
      ) : (
        <>
          <button
            type="button"
            className="shelf-handle"
            aria-expanded={false}
            aria-label={count > 0 ? `展開檔案暫存區，目前 ${count} 個` : '展開檔案暫存區'}
            title="檔案暫存區：先放著，之後再拖進任何對話"
            onClick={() => setOpenPersisted(true)}
          >
            <FileIcon />
            {count > 0 ? <span className="shelf-count">{count}</span> : null}
            <span className="shelf-handle-label">檔案暫存</span>
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

/** One parked image: click hands over, drag carries its key. On touch a tap is the hand-over, so preview is long press. */
function ShelfCard({
  item,
  sinkLabel,
  peek,
  onPeekStart,
  onPeekEnd,
}: {
  item: ShelfItem
  sinkLabel: string | null
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
      notify('error', '先開一個對話（bot 或群組），才能把暫存的檔案放進去。')
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
    // 非圖片沒有預覽可開，長按就不該吃掉那一下點擊。
    if (e.pointerType === 'mouse' || !item.isImage) return
    const el = e.currentTarget
    endPress()
    press.current = {
      x: e.clientX,
      y: e.clientY,
      timer: window.setTimeout(() => {
        press.current = null
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
          onPeekEnd()
        }}
        onPointerEnter={(e) => {
          if (e.pointerType === 'mouse' && item.isImage) onPeekStart(item.key, e.currentTarget, PEEK_HOVER_MS, false)
        }}
        onPointerLeave={(e) => {
          if (e.pointerType === 'mouse') onPeekEnd()
          endPress()
        }}
        onPointerDown={onPointerDown}
        onPointerUp={endPress}
        onPointerCancel={endPress}
        onPointerMove={(e) => {
          if (!press.current) return
          if (Math.abs(e.clientX - press.current.x) > 10 || Math.abs(e.clientY - press.current.y) > 10) endPress()
        }}
        onFocus={(e) => item.isImage && onPeekStart(item.key, e.currentTarget, 0, false)}
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
        {item.isImage ? (
          <img src={item.url} alt="" />
        ) : (
          <span className="shelf-card-file" aria-hidden="true">
            <FileIcon />
          </span>
        )}
        <span className="shelf-card-name">{item.name}</span>
        <span className="shelf-card-size">{handed ? '已放入' : formatSize(item.size)}</span>
      </button>
      <button type="button" className="shelf-card-x" aria-label={`從暫存區移除 ${item.name}`} title="移除" onClick={() => remove(item.key)}>
        ×
      </button>
    </div>
  )
}

/** Floating preview outside the shelf; measured in a layout effect, height capped by `--peek-avail` so short landscape phones don't clamp it onto the strip. */
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
      setPos({
        left: clamp(at.left + at.width / 2 - w / 2, 8, Math.max(8, window.innerWidth - w - 8)),
        top: clamp(at.boundTop - 10 - h, 8, Math.max(8, at.boundTop - 10 - h)),
      })
    } else {
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
    width: `min(${peekWidth()}px, calc(100vw - 24px))`,
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
