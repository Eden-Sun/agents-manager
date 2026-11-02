import { useEffect, useRef } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import * as api from '../api'
import { ApiError } from '../api/types'
import { copyText } from '../lib/copyText'
import { t } from '../lib/i18n'
import { cursorSeq, planFrame, splitFrame } from '../lib/shellFrame'
import { createInputBatcher } from '../lib/xtermInput'
import type { ShellOp } from '../lib/xtermInput'
import { useStore } from '../store/store'
import './hostShellXterm.css'

/**
 * 主機 shell「鍵盤直通」的終端：xterm.js 取代快照 `<pre>`（桌面限定；手機仍用一行輸入）。
 *
 * - 尺寸：xterm 的 cols×rows 由 fit addon 依面板大小定（容器不會被撐出去，聚焦時瀏覽器也不會捲走畫面）。
 *   herdr pane 比面板高時取**最底下**的那段（shell 的提示列在底下）；寬時「換行」決定折行或截掉右邊。
 *   pane 的實際大小只拿來顯示在標題列。
 * - 畫面：優先連 `…/stream` 收推送 frame；連不上（舊 daemon 404、mock）就每 250ms 輪詢 `terminal?format=ansi`。
 * - 輸入：xterm `onData` → `createInputBatcher` → text 走 `text`（enter=false）、控制鍵走 `keys`，都帶 `echo=ansi`，
 *   回應的畫面立刻畫上；比已畫的舊（序號較小）就丟掉。
 */

const POLL_MS = 250
/** 連上又被關（代理、daemon 重啟）：很快重連一次。握手就被拒：慢一點，免得舊 daemon 上每秒打一發 404。 */
const STREAM_REOPEN_MS = 1_000
const STREAM_RETRY_MS = 5_000
/** DECAWM：自動折行開／關。「不折行」時超出右邊的字截掉。 */
const AUTOWRAP_ON = '\x1b[?7h'
const AUTOWRAP_OFF = '\x1b[?7l'

/** 同 `termLinks`：終端裡的 URL 點一下複製（多半是一次性登入連結）。 */
const URL_RE = /https?:\/\/[^\s<>"'`）」』、。，；：！？》〉】〕“”‘’]+/g

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
  /** 「換行」勾選：開＝超出右邊的字折到下一列；關＝截掉。 */
  wrap: boolean
  onTyping: (on: boolean) => void
  /** 讀畫面的結果：成功 `null`，失敗是錯誤（呼叫端判斷 404 pane 不見、403 唯讀）。 */
  onRead: (err: unknown | null) => void
  /** 送字／送鍵的結果：成功 `null`，失敗是錯誤（呼叫端判斷 403）。 */
  onSend: (err: unknown | null) => void
  /** herdr pane 的實際 cols×rows（標題列用；推送與回應都帶）。 */
  onGeometry: (cols: number, rows: number) => void
}

export function HostShellXterm({ host, paneId, wrap, onTyping, onRead, onSend, onGeometry }: HostShellXtermProps) {
  const boxRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<Terminal | null>(null)
  // 回呼每次 parent 重畫都可能換：放 ref，effect 不因此重建 xterm（重建會清掉捲動歷史）。
  const cb = useRef({ onTyping, onRead, onSend, onGeometry, wrap })
  useEffect(() => {
    cb.current = { onTyping, onRead, onSend, onGeometry, wrap }
  })
  useEffect(() => {
    termRef.current?.write(wrap ? AUTOWRAP_ON : AUTOWRAP_OFF)
  }, [wrap])

  useEffect(() => {
    const el = boxRef.current
    if (!el) return
    let alive = true
    /** 404：pane 已經不在，停止輪詢與重連，交給 parent 切走。 */
    let stop = false
    /** 上一次推送是錯誤幀：下一次重連要慢一點。 */
    let lastStreamErr = false

    const term = new Terminal({
      scrollback: 5000,
      cursorBlink: true,
      fontSize: 13,
      fontFamily: getComputedStyle(el).getPropertyValue('--font-mono').trim() || 'monospace',
      theme: readTheme(el),
    })
    termRef.current = term
    const fit = new FitAddon()
    term.loadAddon(fit)
    term.open(el)
    if (!cb.current.wrap) term.write(AUTOWRAP_OFF)
    const ta = term.textarea
    const onFocus = () => cb.current.onTyping(true)
    const onBlur = () => cb.current.onTyping(false)
    ta?.addEventListener('focus', onFocus)
    ta?.addEventListener('blur', onBlur)

    term.registerLinkProvider({
      provideLinks(y, callback) {
        const line = term.buffer.active.getLine(y - 1)?.translateToString(true) ?? ''
        const links: { range: { start: { x: number; y: number }; end: { x: number; y: number } }; text: string; activate: () => void }[] = []
        for (const m of line.matchAll(URL_RE)) {
          const start = (m.index ?? 0) + 1
          links.push({
            range: { start: { x: start, y }, end: { x: start + m[0].length - 1, y } },
            text: m[0],
            activate: () => {
              void copyText(m[0]).then((ok) => {
                if (!ok) useStore.getState().notify('error', t('複製失敗，請手動選取網址'))
              })
            },
          })
        }
        callback(links.length > 0 ? links : undefined)
      },
    })

    /** 最後收到的整張畫面（herdr 的文字）；尺寸變了就拿它重畫。 */
    let lastText = ''
    /** xterm 視窗目前的列（與 `term` 同步）；增量重畫靠它。 */
    let prev: string[] = []
    /** 已經畫上去的最大序號；比它小的（較舊的回應）丟掉。 */
    let appliedId = 0
    let nextId = 0
    let streamOn = false
    let closeStream: (() => void) | null = null
    let streamTimer: ReturnType<typeof setTimeout> | null = null
    let pollTimer: ReturnType<typeof setTimeout> | null = null

    const paint = () => {
      if (!alive) return
      const plan = planFrame(prev, splitFrame(lastText, term.rows))
      if (plan.out) term.write(plan.out + cursorSeq(plan.lines))
      prev = plan.lines
    }
    const draw = (id: number, text: string, pane: { cols: number | null; rows: number | null }) => {
      if (!alive || id < appliedId) return
      appliedId = id
      if (pane.cols && pane.rows) cb.current.onGeometry(pane.cols, pane.rows)
      lastText = text
      paint()
    }
    const reportRead = (e: unknown | null) => {
      if (alive) cb.current.onRead(e)
    }

    /** 依面板大小定 cols×rows；變了就整屏重畫（列數變了 planFrame 會自己整屏畫）。 */
    const fitToBox = () => {
      const before = [term.cols, term.rows]
      try {
        fit.fit()
      } catch {
        /* 隱藏中（沒有尺寸）：下一次 ResizeObserver 再說 */
      }
      if (term.cols !== before[0] || term.rows !== before[1]) {
        prev = []
        paint()
      }
    }
    fitToBox()
    const ro = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(() => fitToBox())
    ro?.observe(el)

    const pollOnce = async () => {
      const id = ++nextId
      try {
        const s = await api.readHostShell(host, paneId, 'visible', 200, 'ansi')
        if (!alive) return
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
      const close = api.openHostShellStream(host, paneId, {
        onMessage: (msg) => {
          const m = (msg ?? {}) as { type?: unknown; text?: unknown; cols?: unknown; rows?: unknown }
          if (m.type === 'frame' && typeof m.text === 'string') {
            streamOn = true
            lastStreamErr = false
            const num = (v: unknown) => (typeof v === 'number' && v > 0 ? v : null)
            draw(++nextId, m.text, { cols: num(m.cols), rows: num(m.rows) })
            reportRead(null)
          } else if (m.type === 'error') {
            // 錯誤幀之後 daemon 會關線；onClose 看到 lastStreamErr 就放慢重連。
            lastStreamErr = true
            streamOn = false
          }
        },
        onClose: (opened) => {
          streamOn = false
          closeStream = null
          if (!alive || stop) return
          streamTimer = setTimeout(startStream, opened && !lastStreamErr ? STREAM_REOPEN_MS : STREAM_RETRY_MS)
        },
      })
      // 沒有 WebSocket 的 transport（mock）：只靠輪詢。
      if (close) closeStream = close
    }

    // 輸入：同一個 frame 內的 onData 合成一次送出；送出串成一條鏈，順序不亂。
    let chain: Promise<void> = Promise.resolve()
    const sendOps = (ops: ShellOp[]) => {
      chain = chain.then(async () => {
        for (const op of ops) {
          const id = ++nextId
          try {
            const snap =
              op.kind === 'text'
                ? await api.sendHostShellText(host, paneId, op.text, false, { echo: true })
                : await api.sendHostShellKeys(host, paneId, op.keys, { echo: true })
            if (!alive) return
            if (snap) draw(id, snap.text, { cols: snap.columns, rows: snap.rows })
            cb.current.onSend(null)
          } catch (e) {
            if (alive) cb.current.onSend(e)
          }
        }
      })
    }
    const batcher = createInputBatcher(sendOps)
    const dataSub = term.onData((data) => batcher(data))

    // 分頁回前景：不等下一拍，立刻補讀一次（沒有推送時）。
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
      closeStream?.()
      document.removeEventListener('visibilitychange', onVisible)
      ro?.disconnect()
      ta?.removeEventListener('focus', onFocus)
      ta?.removeEventListener('blur', onBlur)
      dataSub.dispose()
      term.dispose()
      termRef.current = null
    }
  }, [host, paneId])

  return <div ref={boxRef} className="shell-xterm" />
}
