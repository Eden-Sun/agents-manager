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
import { linksOnRow, wrapWidth, type CellRow } from '../lib/xtermLinks'
import { createInputBatcher } from '../lib/xtermInput'
import type { ShellOp } from '../lib/xtermInput'
import { useStore } from '../store/store'
import './hostShellXterm.css'

/**
 * 主機 shell「鍵盤直通」的終端：xterm.js 取代快照 `<pre>`（桌面限定；手機仍用一行輸入）。
 *
 * 兩種畫面模式（依 daemon 推來的訊息決定，`onObserve` 告訴呼叫端）：
 *
 * - observe（`raw`）：herdr 的終端串流，位元組原封不動寫進 xterm。游標、同步更新都由 ANSI 本身決定，不估游標。
 *   xterm 的 cols×rows 等於 herdr pane 的實際大小（daemon 在幀裡回報；字級固定 13px，放不下就捲動）：observe 不改 pane 寬高、
 *   只把 pane 的格子裁切／補白成要求的尺寸，要求得比 pane 窄，長行每列右邊就被砍掉（不是自動換行的問題）。
 *   連上與每 3 秒讀一次 pane 的實際大小（REST），有變就送 `{type:'size'}` 給 daemon；full 幀先 reset 再寫。「縮放至全螢幕」在這個模式下沒有意義（隱藏）。
 * - frame（舊）：推送整張文字快照（`frame`），或退回每 250ms 輪詢 `terminal?format=ansi`。尺寸與字級照舊：
 *   xterm 的 cols×rows 等於 herdr pane 的實際幾何；不用 fit 決定列數（那樣會把 pane 的列切掉、長行折成兩列，畫面就疊了殘影）。
 *   游標用 `planFrame`＋`cursorSeq` 估。
 *
 * 「換行」對應 DECAWM（pane 寬度與 xterm 一致，理論上不會折行；保險用）。
 * 輸入：xterm `onData` → `createInputBatcher` → text 走 `text`（enter=false）、控制鍵走 `keys`。
 * frame 模式下帶 `echo=ansi`、回應的畫面立刻畫上；observe 模式不要 echo，畫面由串流更新。
 */

const POLL_MS = 250
/** observe 模式：面板尺寸變了，等這麼久才送 size（拖曳縮放時不一直送）。 */
const SIZE_DEBOUNCE_MS = 150
/** observe 模式：多久重讀一次 pane 的實際大小（在 herdr 那邊被拉大／縮小時串流要跟著換）。 */
const PANE_DIMS_MS = 3_000
/** observe 模式的字級（固定；面板大小只決定 cols×rows）。 */
const OBSERVE_FONT = 13
/** 連上又被關（代理、daemon 重啟）：很快重連一次。握手就被拒：慢一點，免得舊 daemon 上每秒打一發 404。 */
const STREAM_REOPEN_MS = 1_000
const STREAM_RETRY_MS = 5_000
/** DECAWM：自動折行開／關。「不折行」時超出右邊的字截掉。 */
const AUTOWRAP_ON = '\x1b[?7h'
const AUTOWRAP_OFF = '\x1b[?7l'

/** 同 `termLinks`：終端裡的 URL 點一下複製（多半是一次性登入連結）。 */
const URL_RE = /https?:\/\/[^\s<>"'`）」』、。，；：！？》〉】〕“”‘’]+/g

/** base64 → bytes（不轉成字串：UTF-8 多位元組字與 ANSI 控制序列交給 xterm 解）。 */
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
  /** 縮放至全螢幕：字級縮到整個 pane 放得下（關＝字級不低於 12px，放不下就捲動）。observe 模式不用。 */
  fitScreen: boolean
  /** 畫面是不是 observe（串流）模式；呼叫端據此隱藏「縮放至全螢幕」。 */
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
  // 回呼每次 parent 重畫都可能換：放 ref，effect 不因此重建 xterm（重建會清掉捲動歷史）。
  const cb = useRef({ onTyping, onRead, onSend, onGeometry, onStreaming, onObserve, wrap, fitScreen })
  /** 目前的字級重算（effect 內設定）；切換縮放時從外面呼叫。 */
  const refitRef = useRef<() => void>(() => {})
  useEffect(() => {
    cb.current = { onTyping, onRead, onSend, onGeometry, onStreaming, onObserve, wrap, fitScreen }
  })
  useEffect(() => {
    refitRef.current()
  }, [fitScreen])
  /** observe 模式中：herdr 已照面板寬折好行、靠終端自動換行接續，「換行」勾選不套用。 */
  const observeRef = useRef(false)
  useEffect(() => {
    if (!observeRef.current) termRef.current?.write(wrap ? AUTOWRAP_ON : AUTOWRAP_OFF)
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
    const fit = new FitAddon()
    term.loadAddon(fit)
    // 畫面上的網址可以點（codex 登入網址之類），折到下一列的長網址也接起來（`lib/xtermLinks`）；新分頁開，不帶 opener。
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
        // observe 模式：pane 比 xterm 欄數窄幾格，折行的列寫不到最後一格；用畫面上最寬的列判斷哪些列是寫滿折行的。
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
        done(
          found.length === 0
            ? undefined
            : found.map((f) => ({
                text: f.url,
                range: { start: { x: f.start.x + 1, y: f.start.y + 1 }, end: { x: f.end.x + 1, y: f.end.y + 1 } },
                activate: () => void window.open(f.url, '_blank', 'noopener,noreferrer'),
              })),
        )
      },
    })
    termRef.current = term
    term.open(el)
    // 先量一次面板：連線時送的第一個 size 就是面板實際大小（observe 模式用）。
    fit.fit()
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
    /** 目前是 observe（串流）模式：收到第一張 `raw` 才定案；`frame` 或斷線就回到 false。 */
    let observe = false
    const setObserve = (on: boolean) => {
      if (observe === on) return
      observe = on
      observeRef.current = on
      if (!alive) return
      // 回到快照模式才照「換行」勾選；observe 一律自動換行（不然折行的長輸出會被截掉）。
      term.write(on || cb.current.wrap ? AUTOWRAP_ON : AUTOWRAP_OFF)
      cb.current.onObserve?.(on)
      // 模式換了：字級與尺寸規則跟著換（observe 固定字級、frame 照 fitScreen 算）。
      fitFont()
    }
    let closeStream: (() => void) | null = null
    let streamTimer: ReturnType<typeof setTimeout> | null = null
    let pollTimer: ReturnType<typeof setTimeout> | null = null
    /** 往目前這條推送線送 JSON；沒有連線（或已斷）就是 null。 */
    let sendStream: ((msg: unknown) => void) | null = null
    let sizeTimer: ReturnType<typeof setTimeout> | null = null
    let dimsTimer: ReturnType<typeof setInterval> | null = null
    /** 最後送給 daemon 的 cols×rows（`colsxrows`）；相同就不重送。 */
    let sentSize = ''

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

    /** 使用者自己打在行尾、還沒被別的輸入蓋過的空白格數（游標估算用）。 */
    let typedSpaces = 0
    let lastCursor = ''
    const paint = () => {
      if (!alive) return
      const plan = planFrame(prev, splitFrame(lastText, term.rows))
      const cur = cursorSeq(plan.lines, typedSpaces)
      // 畫面沒變但游標估算變了（例如打了一個空白）也要移游標。
      if (plan.out || cur !== lastCursor) term.write(plan.out + cur, () => requestAnimationFrame(follow))
      lastCursor = cur
      prev = plan.lines
    }
    /** herdr pane 的實際 cols×rows（REST 讀來的；還沒讀到是 null，先用面板大小頂著）。 */
    let paneDims: { cols: number; rows: number } | null = null
    /** observe 模式：告訴 daemon 要串流多大。必須等於 pane 的實際大小：observe 不改 pane，只裁切／補白，窄了長行每列右邊就沒了。 */
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
    /** 讀 pane 的實際大小；變了就重送 size（pane 在 herdr 那邊被拉大／縮小，串流跟著換）。 */
    const refreshPaneDims = async () => {
      try {
        const s = await api.readHostShell(host, paneId, 'visible', 1)
        if (!alive || !s.columns || !s.rows) return
        if (paneDims?.cols === s.columns && paneDims.rows === s.rows) return
        paneDims = { cols: s.columns, rows: s.rows }
        sendSize(false)
      } catch {
        /* 讀不到就維持原本的尺寸 */
      }
    }
    const scheduleSize = () => {
      if (sizeTimer) clearTimeout(sizeTimer)
      sizeTimer = setTimeout(() => sendSize(false), SIZE_DEBOUNCE_MS)
    }
    const fitObserve = () => {
      if (!alive) return
      // xterm 的 cols×rows 不再跟面板走：herdr observe 不改 pane 寬高，只把 pane 的格子裁切／補白成要求的尺寸，
      // 要求的比 pane 窄就把每列右邊砍掉。所以送給 daemon 的尺寸是 pane 的實際大小，xterm 照幀裡回報的大小 resize（drawRaw），
      // 面板放不下就捲動。size 訊息送的是 pane 的實際大小（sendSize）。
      scheduleSize()
    }
    /** 字級：預設不低於 12px（放不下就捲動）；縮放至全螢幕時縮到整個 pane 放得下。observe 模式改走 fitObserve。 */
    const fitFont = () => {
      if (!alive) return
      if (observe) {
        // observe 模式字級固定；縮放至全螢幕不作用。
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
    /** observe：一張串流幀（full 先清再寫）。位元組原封不動交給 xterm，游標與同步更新都在 ANSI 裡。 */
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
        // 推送已經來過（串流接上）：這趟輪詢是舊的，不蓋過串流畫上的畫面。
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
            // 錯誤幀之後 daemon 會關線；onClose 看到 lastStreamErr 就放慢重連。
            lastStreamErr = true
            setStreamOn(false)
          }
        },
        onOpen: (send) => {
          sendStream = send
          // 連上就告訴 daemon 面板大小（舊 daemon 不認得這則，忽略即可）。
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
            // observe 模式畫面由串流更新，不要 echo（省一趟回應、也不會跟串流搶畫面）。
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
