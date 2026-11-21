import { useEffect, useRef } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import * as api from '../api'
import { ApiError } from '../api/types'
import { copyText } from '../lib/copyText'
import { t } from '../lib/i18n'
import { cursorSeq, planFrame, splitFrame, typedSpacesAfter } from '../lib/shellFrame'
import { fontSizeFor } from '../lib/shellFont'
import { codesOnRow, linksOnRow, wrapWidth, type CellRow } from '../lib/xtermLinks'
import { createInputBatcher } from '../lib/xtermInput'
import type { ShellOp } from '../lib/xtermInput'
import { useStore } from '../store/store'
import './hostShellXterm.css'

const POLL_MS = 250
const SIZE_DEBOUNCE_MS = 150
const PANE_DIMS_MS = 3_000
const OBSERVE_FONT = 13
const STREAM_REOPEN_MS = 1_000
const STREAM_RETRY_MS = 5_000
const AUTOWRAP_ON = '\x1b[?7h'
const AUTOWRAP_OFF = '\x1b[?7l'

function decodeBase64(s: string): Uint8Array {
  const bin = atob(s)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

function readTheme(el: HTMLElement) {
  const cs = getComputedStyle(el)
  const v = (name: string, fallback: string) => cs.getPropertyValue(name).trim() || fallback
  return {
    background: v('--term-bg', '#111111'),
    foreground: v('--term-text', '#dddddd'),
    cursor: v('--accent', '#4a90d9'),
  }
}

export interface HostShellXtermProps {
  host: string
  paneId: string
  wrap: boolean
  onTyping: (on: boolean) => void
  onRead: (err: unknown | null) => void
  onSend: (err: unknown | null) => void
  onGeometry: (cols: number, rows: number) => void
  onStreaming: (on: boolean) => void
  fitScreen: boolean
  onObserve?: (on: boolean) => void
}

export function HostShellXterm({
  host,
  paneId,
  wrap,
  onTyping,
  onRead,
  onSend,
  onGeometry,
  onStreaming,
  fitScreen,
  onObserve,
}: HostShellXtermProps) {
  const boxRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<Terminal | null>(null)
  const cb = useRef({ onTyping, onRead, onSend, onGeometry, onStreaming, onObserve, wrap, fitScreen })
  const refitRef = useRef<() => void>(() => {})
  useEffect(() => {
    cb.current = { onTyping, onRead, onSend, onGeometry, onStreaming, onObserve, wrap, fitScreen }
  })
  useEffect(() => {
    refitRef.current()
  }, [fitScreen])
  const observeRef = useRef(false)
  useEffect(() => {
    if (!observeRef.current) termRef.current?.write(wrap ? AUTOWRAP_ON : AUTOWRAP_OFF)
  }, [wrap])

  useEffect(() => {
    const el = boxRef.current
    if (!el) return
    let alive = true
    let stop = false
    let lastStreamErr = false

    const term = new Terminal({
      scrollback: 5000,
      cursorBlink: false,
      cursorStyle: 'block',
      cursorInactiveStyle: 'outline',
      fontSize: 13,
      fontFamily: getComputedStyle(el).getPropertyValue('--font-mono').trim() || 'monospace',
      theme: readTheme(el),
    })
    const fit = new FitAddon()
    term.loadAddon(fit)
    const linkProvider = term.registerLinkProvider({
      provideLinks(line, done) {
        const buf = term.buffer.active
        const rowAt = (y: number): CellRow | undefined => {
          const l = buf.getLine(y)
          if (!l) return undefined
          const row: CellRow = []
          for (let x = 0; x < term.cols; x++) {
            const c = l.getCell(x)
            row.push(!c ? ' ' : c.getWidth() === 0 ? '' : c.getChars() || ' ')
          }
          return row
        }
        let wrapAt: number | undefined | null
        if (observeRef.current) {
          const top = buf.viewportY
          const rows: CellRow[] = []
          for (let y = top; y < top + term.rows; y++) {
            const r = rowAt(y)
            if (r) rows.push(r)
          }
          wrapAt = wrapWidth(rows, term.cols)
        }
        const found = linksOnRow(rowAt, line - 1, wrapAt)
        const range = (f: { start: { x: number; y: number }; end: { x: number; y: number } }) => ({
          start: { x: f.start.x + 1, y: f.start.y + 1 },
          end: { x: f.end.x + 1, y: f.end.y + 1 },
        })
        const links = found.map((f) => ({
          text: f.url,
          range: range(f),
          activate: () => void window.open(f.url, '_blank', 'noopener,noreferrer'),
        }))
        const row = rowAt(line - 1)
        for (const c of row ? codesOnRow(row, line - 1) : []) {
          links.push({
            text: c.code,
            range: range(c),
            activate: () =>
              void copyText(c.code).then((ok) =>
                ok
                  ? useStore.getState().notify('info', t('已複製一次性碼 {code}', { code: c.code }))
                  : useStore.getState().notify('error', t('複製失敗，請手動選取')),
              ),
          })
        }
        done(links.length === 0 ? undefined : links)
      },
    })
    termRef.current = term
    term.open(el)
    fit.fit()
    if (!cb.current.wrap) term.write(AUTOWRAP_OFF)
    const ta = term.textarea
    const onFocus = () => cb.current.onTyping(true)
    const onBlur = () => cb.current.onTyping(false)
    ta?.addEventListener('focus', onFocus)
    ta?.addEventListener('blur', onBlur)

    let lastText = ''
    let prev: string[] = []
    let appliedId = 0
    let nextId = 0
    let streamOn = false
    const setStreamOn = (on: boolean) => {
      if (streamOn === on) return
      streamOn = on
      if (alive) cb.current.onStreaming(on)
    }
    let observe = false
    const setObserve = (on: boolean) => {
      if (observe === on) return
      observe = on
      observeRef.current = on
      prev = []
      if (!alive) return
      term.write(on || cb.current.wrap ? AUTOWRAP_ON : AUTOWRAP_OFF)
      cb.current.onObserve?.(on)
      fitFont()
    }
    let closeStream: (() => void) | null = null
    let streamTimer: ReturnType<typeof setTimeout> | null = null
    let pollTimer: ReturnType<typeof setTimeout> | null = null
    let sendStream: ((msg: unknown) => void) | null = null
    let sizeTimer: ReturnType<typeof setTimeout> | null = null
    let dimsTimer: ReturnType<typeof setInterval> | null = null
    let sentSize = ''

    let following = true
    let autoTop = -1
    let autoLeft = -1
    const cursorEl = () => el.querySelector('.xterm-cursor') as HTMLElement | null
    const cursorVisible = () => {
      const c = cursorEl()
      if (!c) return true
      const b = el.getBoundingClientRect()
      const r = c.getBoundingClientRect()
      return r.top >= b.top && r.bottom <= b.bottom && r.left >= b.left && r.right <= b.right
    }
    const follow = () => {
      if (!alive || !following) return
      const c = cursorEl()
      if (!c) return
      const b = el.getBoundingClientRect()
      const r = c.getBoundingClientRect()
      const top = r.top - b.top + el.scrollTop
      const left = r.left - b.left + el.scrollLeft
      let nt = el.scrollTop
      let nl = el.scrollLeft
      if (top < el.scrollTop) nt = top - 8
      else if (top + r.height > el.scrollTop + el.clientHeight) nt = top + r.height - el.clientHeight + 8
      if (left < el.scrollLeft) nl = left - 8
      else if (left + r.width > el.scrollLeft + el.clientWidth) nl = left + r.width - el.clientWidth + 8
      if (nt !== el.scrollTop) el.scrollTop = nt
      if (nl !== el.scrollLeft) el.scrollLeft = nl
      autoTop = el.scrollTop
      autoLeft = el.scrollLeft
    }
    const onScroll = () => {
      if (el.scrollTop === autoTop && el.scrollLeft === autoLeft) return
      following = cursorVisible()
    }
    el.addEventListener('scroll', onScroll, { passive: true })

    let typedSpaces = 0
    let lastCursor = ''
    const paint = () => {
      if (!alive) return
      const plan = planFrame(prev, splitFrame(lastText, term.rows))
      const cur = cursorSeq(plan.lines, typedSpaces)
      if (plan.out || cur !== lastCursor) term.write(plan.out + cur, () => requestAnimationFrame(follow))
      lastCursor = cur
      prev = plan.lines
    }
    let paneDims: { cols: number; rows: number } | null = null
    const sendSize = (force: boolean) => {
      sizeTimer = null
      if (!alive || !sendStream) return
      const d = paneDims ?? fit.proposeDimensions()
      if (!d || !Number.isFinite(d.cols) || !Number.isFinite(d.rows) || d.cols < 1 || d.rows < 1) return
      const key = `${d.cols}x${d.rows}`
      if (!force && key === sentSize) return
      sentSize = key
      sendStream({ type: 'size', cols: d.cols, rows: d.rows })
    }
    const refreshPaneDims = async () => {
      try {
        const s = await api.readHostShell(host, paneId, 'visible', 1)
        if (!alive || !s.columns || !s.rows) return
        if (paneDims?.cols === s.columns && paneDims.rows === s.rows) return
        paneDims = { cols: s.columns, rows: s.rows }
        sendSize(false)
      } catch {
      }
    }
    const scheduleSize = () => {
      if (sizeTimer) clearTimeout(sizeTimer)
      sizeTimer = setTimeout(() => sendSize(false), SIZE_DEBOUNCE_MS)
    }
    const fitObserve = () => {
      if (!alive) return
      scheduleSize()
    }
    const fitFont = () => {
      if (!alive) return
      if (observe) {
        if (term.options.fontSize !== OBSERVE_FONT) term.options.fontSize = OBSERVE_FONT
        fitObserve()
        return
      }
      const next = fontSizeFor(
        { width: el.clientWidth - 16, height: el.clientHeight - 12 },
        { cols: term.cols, rows: term.rows },
        cb.current.fitScreen,
      )
      if (term.options.fontSize !== next) {
        term.options.fontSize = next
        requestAnimationFrame(follow)
      }
    }
    refitRef.current = fitFont
    const draw = (id: number, text: string, pane: { cols: number | null; rows: number | null }) => {
      if (!alive || id < appliedId) return
      appliedId = id
      if (pane.cols && pane.rows) {
        cb.current.onGeometry(pane.cols, pane.rows)
        if (pane.cols !== term.cols || pane.rows !== term.rows) {
          term.resize(pane.cols, pane.rows)
          prev = []
          fitFont()
        }
      }
      lastText = text
      paint()
    }
    const drawRaw = (full: boolean, pane: { cols: number | null; rows: number | null }, bytes: Uint8Array) => {
      if (!alive) return
      setObserve(true)
      if (full) {
        term.reset()
        term.write(AUTOWRAP_ON)
      }
      if (pane.cols && pane.rows) {
        cb.current.onGeometry(pane.cols, pane.rows)
        if (pane.cols !== term.cols || pane.rows !== term.rows) term.resize(pane.cols, pane.rows)
      }
      term.write(bytes, () => requestAnimationFrame(follow))
    }
    const reportRead = (e: unknown | null) => {
      if (alive) cb.current.onRead(e)
    }

    const ro = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(() => {
      fitFont()
      requestAnimationFrame(follow)
    })
    ro?.observe(el)

    const pollOnce = async () => {
      const id = ++nextId
      try {
        const s = await api.readHostShell(host, paneId, 'visible', 200, 'ansi')
        if (!alive || streamOn) return
        draw(id, s.text, { cols: s.columns, rows: s.rows })
        reportRead(null)
      } catch (e) {
        if (e instanceof ApiError && e.status === 404) stop = true
        reportRead(e)
      }
    }
    const pollLoop = async () => {
      pollTimer = null
      if (!alive || stop) return
      if (!streamOn && document.visibilityState !== 'hidden') await pollOnce()
      if (alive && !stop) pollTimer = setTimeout(() => void pollLoop(), POLL_MS)
    }

    const startStream = () => {
      streamTimer = null
      if (!alive || stop) return
      const num = (v: unknown) => (typeof v === 'number' && v > 0 ? v : null)
      const close = api.openHostShellStream(host, paneId, {
        onMessage: (msg) => {
          const m = (msg ?? {}) as { type?: unknown; text?: unknown; data?: unknown; full?: unknown; cols?: unknown; rows?: unknown }
          if (m.type === 'raw' && typeof m.data === 'string') {
            let bytes: Uint8Array
            try {
              bytes = decodeBase64(m.data)
            } catch {
              return
            }
            setStreamOn(true)
            lastStreamErr = false
            drawRaw(m.full === true, { cols: num(m.cols), rows: num(m.rows) }, bytes)
            reportRead(null)
          } else if (m.type === 'frame' && typeof m.text === 'string') {
            setObserve(false)
            setStreamOn(true)
            lastStreamErr = false
            draw(++nextId, m.text, { cols: num(m.cols), rows: num(m.rows) })
            reportRead(null)
          } else if (m.type === 'error') {
            lastStreamErr = true
            setStreamOn(false)
          }
        },
        onOpen: (send) => {
          sendStream = send
          sentSize = ''
          sendSize(true)
          void refreshPaneDims()
          if (dimsTimer) clearInterval(dimsTimer)
          dimsTimer = setInterval(() => void refreshPaneDims(), PANE_DIMS_MS)
        },
        onClose: (opened) => {
          sendStream = null
          if (sizeTimer) clearTimeout(sizeTimer)
          sizeTimer = null
          if (dimsTimer) clearInterval(dimsTimer)
          dimsTimer = null
          setObserve(false)
          setStreamOn(false)
          closeStream = null
          if (!alive || stop) return
          streamTimer = setTimeout(startStream, opened && !lastStreamErr ? STREAM_REOPEN_MS : STREAM_RETRY_MS)
        },
      })
      if (close) closeStream = close
    }

    let queue: ShellOp[] = []
    let flying = false
    const pump = async () => {
      if (flying) return
      flying = true
      try {
        while (alive && queue.length > 0) {
          const batch = queue
          queue = []
          for (let k = 0; k < batch.length && alive; k++) {
            const op = batch[k]
            const echo = k === batch.length - 1 && !observe
            const id = echo ? ++nextId : 0
            typedSpaces = typedSpacesAfter(typedSpaces, op)
            try {
              const snap =
                op.kind === 'text'
                  ? await api.sendHostShellText(host, paneId, op.text, false, { echo })
                  : await api.sendHostShellKeys(host, paneId, op.keys, { echo })
              if (!alive) return
              if (snap && !observe) draw(id, snap.text, { cols: snap.columns, rows: snap.rows })
              cb.current.onSend(null)
            } catch (e) {
              if (alive) cb.current.onSend(e)
            }
          }
        }
      } finally {
        flying = false
      }
    }
    const sendOps = (ops: ShellOp[]) => {
      queue.push(...ops)
      void pump()
    }
    const batcher = createInputBatcher(sendOps)
    const dataSub = term.onData((data) => batcher(data))

    const onVisible = () => {
      if (document.visibilityState === 'visible' && !streamOn && alive && !stop) void pollOnce()
    }
    document.addEventListener('visibilitychange', onVisible)

    startStream()
    void pollLoop()
    term.focus()

    return () => {
      alive = false
      if (pollTimer) clearTimeout(pollTimer)
      if (streamTimer) clearTimeout(streamTimer)
      if (sizeTimer) clearTimeout(sizeTimer)
      if (dimsTimer) clearInterval(dimsTimer)
      sendStream = null
      closeStream?.()
      document.removeEventListener('visibilitychange', onVisible)
      ro?.disconnect()
      el.removeEventListener('scroll', onScroll)
      refitRef.current = () => {}
      ta?.removeEventListener('focus', onFocus)
      ta?.removeEventListener('blur', onBlur)
      dataSub.dispose()
      linkProvider.dispose()
      term.dispose()
      termRef.current = null
    }
  }, [host, paneId])

  return <div ref={boxRef} className="shell-xterm" />
}
