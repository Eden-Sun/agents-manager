import { useEffect, useRef } from 'react'
import { Terminal } from '@xterm/xterm'
import '@xterm/xterm/css/xterm.css'
import * as api from '../api'
import { ApiError } from '../api/types'
import { copyText } from '../lib/copyText'
import { t } from '../lib/i18n'
import { cursorSeq, planFrame, splitFrame } from '../lib/shellFrame'
import { fontSizeFor } from '../lib/shellFont'
import { createInputBatcher } from '../lib/xtermInput'
import type { ShellOp } from '../lib/xtermInput'
import { useStore } from '../store/store'
import './hostShellXterm.css'

/**
 * 主機 shell「鍵盤直通」的終端：xterm.js 取代快照 `<pre>`（桌面限定；手機仍用一行輸入）。
 *
 * - 尺寸：xterm 的 cols×rows 一律等於 herdr pane 的實際幾何（frame 帶來）。面板放不下就縮字級，還放不下就容器捲動；
 *   不用 fit 決定列數（那樣會把 pane 的列切掉、長行折成兩列，畫面就疊了殘影）。
 *   「換行」對應 DECAWM（pane 寬度與 xterm 一致，理論上不會折行；保險用）。
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
  /** 目前是推送（true）還是輪詢退回（false）；狀態列據此寫。 */
  onStreaming: (on: boolean) => void
  /** 縮放至全螢幕：字級縮到整個 pane 放得下（關＝字級不低於 12px，放不下就捲動）。 */
  fitScreen: boolean
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
}: HostShellXtermProps) {
  const boxRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<Terminal | null>(null)
  // 回呼每次 parent 重畫都可能換：放 ref，effect 不因此重建 xterm（重建會清掉捲動歷史）。
  const cb = useRef({ onTyping, onRead, onSend, onGeometry, onStreaming, wrap, fitScreen })
  /** 目前的字級重算（effect 內設定）；切換縮放時從外面呼叫。 */
  const refitRef = useRef<() => void>(() => {})
  useEffect(() => {
    cb.current = { onTyping, onRead, onSend, onGeometry, onStreaming, wrap, fitScreen }
  })
  useEffect(() => {
    refitRef.current()
  }, [fitScreen])
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
      cursorBlink: false,
      cursorStyle: 'block',
      cursorInactiveStyle: 'outline',
      fontSize: 13,
      fontFamily: getComputedStyle(el).getPropertyValue('--font-mono').trim() || 'monospace',
      theme: readTheme(el),
    })
    termRef.current = term
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
    const setStreamOn = (on: boolean) => {
      if (streamOn === on) return
      streamOn = on
      if (alive) cb.current.onStreaming(on)
    }
    let closeStream: (() => void) | null = null
    let streamTimer: ReturnType<typeof setTimeout> | null = null
    let pollTimer: ReturnType<typeof setTimeout> | null = null

    // 自動跟隨：游標在視窗外就捲過去。使用者自己往上捲、游標移出視窗時暫停；捲回游標所在處恢復。
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
      // 自己捲的不算：只有使用者捲動才改跟隨狀態。
      if (el.scrollTop === autoTop && el.scrollLeft === autoLeft) return
      following = cursorVisible()
    }
    el.addEventListener('scroll', onScroll, { passive: true })

    const paint = () => {
      if (!alive) return
      const plan = planFrame(prev, splitFrame(lastText, term.rows))
      if (plan.out) term.write(plan.out + cursorSeq(plan.lines), () => requestAnimationFrame(follow))
      prev = plan.lines
    }
    /** 字級：預設不低於 12px（放不下就捲動）；縮放至全螢幕時縮到整個 pane 放得下。 */
    const fitFont = () => {
      if (!alive) return
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
            setStreamOn(true)
            lastStreamErr = false
            const num = (v: unknown) => (typeof v === 'number' && v > 0 ? v : null)
            draw(++nextId, m.text, { cols: num(m.cols), rows: num(m.rows) })
            reportRead(null)
          } else if (m.type === 'error') {
            // 錯誤幀之後 daemon 會關線；onClose 看到 lastStreamErr 就放慢重連。
            lastStreamErr = true
            setStreamOn(false)
          }
        },
        onClose: (opened) => {
          setStreamOn(false)
          closeStream = null
          if (!alive || stop) return
          streamTimer = setTimeout(startStream, opened && !lastStreamErr ? STREAM_REOPEN_MS : STREAM_RETRY_MS)
        },
      })
      // 沒有 WebSocket 的 transport（mock）：只靠輪詢。
      if (close) closeStream = close
    }

    // 輸入：送出是「一次只飛一個請求」的泵（順序保證），但不逐個等回應再送下一個：
    // 飛行中打的字先排進佇列，這一趟結束就整批送出。遠端（echo 約 220ms）打快時，每趟自然把期間的字合成一批，
    // 延遲不會隨打字量累積。只有每批的最後一個操作要 echo（畫面回應）；中間的只要確認送達。
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
            const echo = k === batch.length - 1
            const id = echo ? ++nextId : 0
            try {
              const snap =
                op.kind === 'text'
                  ? await api.sendHostShellText(host, paneId, op.text, false, { echo })
                  : await api.sendHostShellKeys(host, paneId, op.keys, { echo })
              if (!alive) return
              if (snap) draw(id, snap.text, { cols: snap.columns, rows: snap.rows })
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
      el.removeEventListener('scroll', onScroll)
      refitRef.current = () => {}
      ta?.removeEventListener('focus', onFocus)
      ta?.removeEventListener('blur', onBlur)
      dataSub.dispose()
      term.dispose()
      termRef.current = null
    }
  }, [host, paneId])

  return <div ref={boxRef} className="shell-xterm" />
}
