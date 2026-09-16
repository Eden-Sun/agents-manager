import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { ClipboardEvent as ReactClipboardEvent, KeyboardEvent as ReactKeyboardEvent } from 'react'
import * as api from '../api'
import { herdrKeyFromEvent, useShellKeys } from '../hooks/usePaneKeys'
import { ApiError } from '../api/types'
import type { TerminalSnapshot, TerminalSource } from '../api/types'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { HostBadge } from './HostsPanel'
import { linkifyTerm } from './TermLinks'
import { setTermWrap, useTermWrap } from './termWrap'
import './hostShellPanel.css'

/**
 * 對某台主機開著的 shell：終端快照 + 一行指令輸入；不是 bot（沒有 run／回合／訊息）。
 * 輪詢不重用 `useTerminalSnapshot`：它吃 `botId`，改簽名會動到 ChatPanel／BlockedModal；節奏與清畫面照它。
 */

/**
 * 鍵盤同步（每個 pane 各記一份，localStorage）：開著時終端本身收鍵盤，每一下原樣送進那個 pane，
 * 就像坐在那台終端前面。關著時是原本的「打一行、Enter 送出」。
 */
const SYNC_KEY = 'am.shellKeySync'
/** 同步時輪詢要快一點，不然自己打的字要等一秒才看得到。 */
const SYNC_POLL_MS = 250
const IDLE_POLL_MS = 1_000

function readSyncSet(): Set<string> {
  try {
    const raw = localStorage.getItem(SYNC_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    return new Set(Array.isArray(parsed) ? parsed.filter((x): x is string => typeof x === 'string') : [])
  } catch {
    return new Set()
  }
}

function writeSync(target: string, on: boolean) {
  try {
    const set = readSyncSet()
    if (on) set.add(target)
    else set.delete(target)
    localStorage.setItem(SYNC_KEY, JSON.stringify([...set]))
  } catch {
    /* storage unavailable: 這一頁還記得 */
  }
}

/** 每台主機各記一份指令歷史（localStorage）。 */
const HISTORY_KEY = 'am.shellHistory'
const HISTORY_MAX = 20

type HistoryMap = Record<string, string[]>

function readHistory(): HistoryMap {
  try {
    const raw = localStorage.getItem(HISTORY_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {}
    const out: HistoryMap = {}
    for (const [host, v] of Object.entries(parsed as Record<string, unknown>)) {
      if (Array.isArray(v)) out[host] = v.filter((x): x is string => typeof x === 'string').slice(0, HISTORY_MAX)
    }
    return out
  } catch {
    return {}
  }
}

function writeHistory(map: HistoryMap) {
  try {
    localStorage.setItem(HISTORY_KEY, JSON.stringify(map))
  } catch {
    /* storage unavailable: 這一頁還記得，重整就沒了 */
  }
}

/** 未送出的指令依 `host/paneId` 各存一份，切去看一眼對話不會丟。 */
const DRAFT_KEY = 'am.shellDrafts'

function readDrafts(): Record<string, string> {
  try {
    const raw = localStorage.getItem(DRAFT_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {}
    const out: Record<string, string> = {}
    for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) if (typeof v === 'string' && v) out[k] = v
    return out
  } catch {
    return {}
  }
}

function writeDraft(key: string, text: string) {
  try {
    const map = readDrafts()
    if (text) map[key] = text
    else delete map[key]
    localStorage.setItem(DRAFT_KEY, JSON.stringify(map))
  } catch {
    /* storage unavailable */
  }
}

/** 送給 daemon（再原樣送 herdr）的鍵名，同 `usePaneKeys` 的契約。 */
const KEYS: { label: string; keys: string[]; title: string }[] = [
  { label: 'ctrl+c', keys: ['ctrl+c'], title: '中斷正在跑的指令' },
  { label: 'Esc', keys: ['esc'], title: '送出 Esc' },
  { label: 'Tab', keys: ['tab'], title: '送出 Tab（讓 shell 自己補完）' },
  { label: '↑', keys: ['up'], title: '送出 ↑（shell 自己的歷史）' },
  { label: 'Enter', keys: ['enter'], title: '只按 Enter' },
]

/** 標題列只放 cwd 最後兩段（完整路徑在 tooltip）；倒數第二段手機用 CSS 藏掉。 */
function cwdTail(cwd: string): { parent: string; leaf: string } {
  const seg = cwd.split('/').filter(Boolean)
  if (seg.length === 0) return { parent: '', leaf: cwd || '/' }
  return { parent: seg.length > 1 ? seg[seg.length - 2] : '', leaf: seg[seg.length - 1] }
}

export function HostShellPanel({
  host,
  paneId,
  cwd,
  onOpenSidebar,
  embedded = false,
}: {
  host: string
  paneId: string
  cwd: string
  onOpenSidebar?: () => void
  /** 掛在 ChatPanel 標題列底下當分頁：不畫自己的 `main-head`，主機與關閉鍵改放抓法列。 */
  embedded?: boolean
}) {
  const closeShellView = useStore((s) => s.closeShellView)
  const endHostShell = useStore((s) => s.endHostShell)
  // 從選單點進來的 pane（§6.5e）：服務 pane 唯讀；不是這個面板開的就不給「結束 shell」。
  const readOnly = useStore((s) => Boolean(s.shellView?.host === host && s.shellView.paneId === paneId && s.shellView.readOnly))
  const traced = useStore((s) => Boolean(s.shellView?.host === host && s.shellView.paneId === paneId && s.shellView.traced))
  const hostUp = useStore((s) => (host === 'local' ? s.connected : (s.hosts.find((h) => h.name === host)?.connected ?? false)))
  const ending = useStore((s) => Boolean(s.busy[`shell:${host}:${paneId}`]))

  const [snap, setSnap] = useState<TerminalSnapshot | null>(null)
  const [err, setErr] = useState<string | null>(null)
  /** 改它就取消排著的那次、立刻重讀（送完指令要馬上看到反應）。 */
  const [nonce, setNonce] = useState(0)
  /** `visible` = 終端現在長什麼樣（shell 的常態）；`recent_unwrapped` = 連捲上去的一起看。 */
  const [source, setSource] = useState<TerminalSource>('visible')
  const [lines, setLines] = useState(200)
  const wrap = useTermWrap()
  const target = `${host}/${paneId}`
  const [text, setTextState] = useState(() => readDrafts()[target] ?? '')
  const setText = useCallback(
    (v: string) => {
      setTextState(v)
      writeDraft(target, v)
    },
    [target],
  )
  const [sending, setSending] = useState(false)
  const [sync, setSyncState] = useState(() => readSyncSet().has(target))
  /** 有沒有真的握著鍵盤：同步開著但焦點在別處時，打字不會進到 pane，要講清楚。 */
  const [typing, setTyping] = useState(false)
  const termRef = useRef<HTMLPreElement>(null)
  const [confirmEnd, setConfirmEnd] = useState(false)
  const [history, setHistory] = useState<HistoryMap>(readHistory)
  /** 在歷史裡的位置；`-1` = 正在編輯的那一行（還沒往上翻）。 */
  const [histAt, setHistAt] = useState(-1)
  const inputRef = useRef<HTMLInputElement>(null)

  // 換 shell 在 render 當下清畫面，不走 effect：留著另一台機器的畫面會被誤認成這一台。
  const [lastTarget, setLastTarget] = useState(target)
  if (lastTarget !== target) {
    setLastTarget(target)
    setSnap(null)
    setErr(null)
    setTextState(readDrafts()[target] ?? '')
    setHistAt(-1)
    setSyncState(readSyncSet().has(target))
    setTyping(false)
  }

  useEffect(() => {
    let alive = true
    let timer: ReturnType<typeof setTimeout> | null = null
    const tick = async () => {
      try {
        const s = await api.readHostShell(host, paneId, source, lines)
        if (alive) {
          setSnap(s)
          setErr(null)
        }
      } catch (e) {
        if (e instanceof ApiError && e.status === 404) {
          if (alive) closeShellView()
          return
        }
        if (alive) setErr(e instanceof Error ? e.message : String(e))
      }
      if (alive) timer = setTimeout(() => void tick(), sync ? SYNC_POLL_MS : IDLE_POLL_MS)
    }
    void tick()
    return () => {
      alive = false
      if (timer) clearTimeout(timer)
    }
  }, [closeShellView, host, paneId, source, lines, nonce, sync])

  const refresh = useCallback(() => setNonce((n) => n + 1), [])

  useEffect(() => {
    if (sync) termRef.current?.focus()
    else inputRef.current?.focus()
  }, [host, paneId, sync])

  const remember = useCallback(
    (cmd: string) => {
      if (!cmd.trim()) return
      setHistory((h) => {
        const prev = h[host] ?? []
        const next = [cmd, ...prev.filter((x) => x !== cmd)].slice(0, HISTORY_MAX)
        const map = { ...h, [host]: next }
        writeHistory(map)
        return map
      })
    },
    [host],
  )

  const run = useCallback(
    async (cmd: string) => {
      setSending(true)
      try {
        await api.sendHostShellText(host, paneId, cmd, true)
        remember(cmd)
        setText('')
        setHistAt(-1)
        setErr(null)
        refresh()
      } catch (e) {
        setErr(e instanceof Error ? e.message : String(e))
      } finally {
        setSending(false)
      }
    },
    [host, paneId, refresh, remember, setText],
  )

  const pressKeys = useCallback(
    async (keys: string[]) => {
      try {
        await api.sendHostShellKeys(host, paneId, keys)
        setErr(null)
        refresh()
      } catch (e) {
        setErr(e instanceof Error ? e.message : String(e))
      }
    },
    [host, paneId, refresh],
  )

  const { press: pressSync, paste: pasteSync } = useShellKeys(
    host,
    paneId,
    useCallback(
      (e: unknown | null) => {
        setErr(e ? (e instanceof Error ? e.message : String(e)) : null)
        refresh()
      },
      [refresh],
    ),
  )

  const setSync = useCallback(
    (on: boolean) => {
      setSyncState(on)
      writeSync(target, on)
      if (on) requestAnimationFrame(() => termRef.current?.focus())
      else requestAnimationFrame(() => inputRef.current?.focus())
    },
    [target],
  )

  /**
   * 同步模式的核心：每一下 keydown 直接變成 herdr 鍵名送進 pane。
   * `herdrKeyFromEvent` 回 `null` 的（⌘ 系列、herdr 不收的 Delete／Home／End／PgUp）留給瀏覽器，
   * 所以 ⌘C／⌘R／⌘V 照常，使用者不會被關在這個框裡出不去。
   */
  const onTermKeyDown = (e: ReactKeyboardEvent<HTMLPreElement>) => {
    if (!sync) return
    const key = herdrKeyFromEvent(e.nativeEvent)
    if (!key) return
    e.preventDefault()
    pressSync([key])
  }

  const onTermPaste = (e: ReactClipboardEvent<HTMLPreElement>) => {
    if (!sync) return
    const text = e.clipboardData.getData('text')
    if (!text) return
    e.preventDefault()
    pasteSync(text)
  }

  const onKeyDown = (e: ReactKeyboardEvent<HTMLInputElement>) => {
    if (e.nativeEvent.isComposing) return
    if (e.key === 'Enter') {
      e.preventDefault()
      if (!sending) void run(text)
      return
    }
    const list = history[host] ?? []
    if (e.key === 'ArrowUp' && list.length > 0) {
      e.preventDefault()
      const at = Math.min(histAt + 1, list.length - 1)
      setHistAt(at)
      setText(list[at])
      return
    }
    if (e.key === 'ArrowDown' && histAt >= 0) {
      e.preventDefault()
      const at = histAt - 1
      setHistAt(at)
      setText(at < 0 ? '' : list[at])
    }
  }

  const body = useMemo(() => {
    if (!snap) return err ? `讀取終端失敗：${err}` : '讀取中…'
    // herdr 的 `recent_unwrapped` 只給已捲出畫面的部分，剛開的 shell 回空字串（實測 2026-09-07）；明講以免看起來像壞掉。
    if (!snap.text.trim() && source === 'recent_unwrapped') {
      return '（還沒有捲出畫面的內容——這個 shell 的輸出目前都還在「畫面」裡。）'
    }
    return linkifyTerm(snap.text, snap.columns)
  }, [err, snap, source])

  const headActions = (
    <div className="head-actions">
      <button type="button" className="mini-btn" onClick={closeShellView} title="只關掉這個畫面，shell 留著">
        關閉
      </button>
      {/* 被 trace 的 pane 不是這裡開的：daemon 的 DELETE 只認自己開的那幾顆，按了會是一顆什麼都不做的按鈕。 */}
      {traced ? null : (
        <button
          type="button"
          className="mini-btn danger"
          disabled={ending}
          onClick={() => setConfirmEnd(true)}
          title="關掉這個 shell 的 pane"
        >
          {ending ? '結束中…' : '結束 shell'}
        </button>
      )}
    </div>
  )

  return (
    <>
      {embedded ? null : (
      <div className="main-head shell-head">
        <button
          type="button"
          className="btn menu-btn icon-tip"
          onClick={onOpenSidebar}
          aria-label="開啟側邊欄"
          data-tip="開啟側邊欄"
        >
          ☰
        </button>
        <div className="main-title">
          <span className="shell-icon" aria-hidden="true">
            ❯
          </span>
          <strong>{host === 'local' ? '本機 shell' : `${host} shell`}</strong>
          <HostBadge host={host} connected={hostUp} />
          <span className="shell-cwd mono" title={cwd}>
            {cwdTail(cwd).parent ? <span className="shell-cwd-parent">{cwdTail(cwd).parent}/</span> : null}
            {cwdTail(cwd).leaf}
          </span>
        </div>
        <span className="spacer" />
        {headActions}
      </div>
      )}

      <div className="shell-pane">
        <div className="term-bar shell-bar">
          {embedded ? (
            <span className="hint" title={cwd}>
              <strong>{host === 'local' ? '本機 shell' : `${host} shell`}</strong>
              <HostBadge host={host} connected={hostUp} />
              <span className="shell-cwd mono">{cwdTail(cwd).leaf}</span>
            </span>
          ) : null}
          <label className="conn">
            檢視
            <select value={source} onChange={(e) => setSource(e.target.value as TerminalSource)}>
              <option value="visible">畫面</option>
              <option value="recent_unwrapped">含捲動歷史</option>
            </select>
          </label>
          {source === 'recent_unwrapped' ? (
            <label className="conn">
              行數
              <select value={lines} onChange={(e) => setLines(Number(e.target.value))}>
                {[100, 200, 500, 1000].map((n) => (
                  <option key={n} value={n}>
                    {n}
                  </option>
                ))}
              </select>
            </label>
          ) : null}
          <button type="button" className="mini-btn" onClick={refresh} title="立刻重讀一次（平常每秒自己更新）">
            刷新
          </button>
          <button
            type="button"
            className={`mini-btn${sync ? ' on' : ''}`}
            aria-pressed={sync}
            disabled={readOnly}
            onClick={() => setSync(!sync)}
            title={
              sync
                ? '關掉鍵盤同步，回到「打一行、Enter 送出」'
                : '鍵盤同步：點終端之後每一下按鍵直接送進這個 pane（⌘ 系列留給瀏覽器）'
            }
          >
            {sync ? '鍵盤同步中' : '鍵盤同步'}
          </button>
          <label className="conn" title="折行後 TUI 畫的框線與對齊會跑掉，但整行讀得到；不折行則維持原樣，靠橫捲看右半邊。">
            <input type="checkbox" checked={wrap} onChange={(e) => setTermWrap(e.target.checked)} />
            換行
          </label>
          <span className="hint term-pane-chip" title={`herdr pane ${paneId}`}>
            pane <code>{paneId}</code>
            {snap?.columns ? `・${snap.columns}×${snap.rows ?? '?'}` : ''}
          </span>
          {snap?.truncated ? <span className="hint">已截斷</span> : null}
          <span className="spacer" />
          <span className="hint term-bar-note">
            {sync ? '鍵盤同步中，每 0.25 秒更新' : '每秒更新，指令送出後立刻重讀'}
          </span>
          {embedded ? headActions : null}
        </div>

        {err && snap ? (
          <div className="shell-err" role="status">
            {/* 有快照卻讀失敗：畫面是舊的。 */}
            最後一次讀取失敗（{err}）——上面顯示的是先前的畫面。
          </div>
        ) : null}

        <pre
          ref={termRef}
          className={`term shell-term${wrap ? ' term-wrap' : ''}${sync ? ' term-sync' : ''}${sync && typing ? ' term-sync-live' : ''}`}
          tabIndex={sync ? 0 : -1}
          role={sync ? 'textbox' : undefined}
          aria-label={sync ? `${host === 'local' ? '本機' : host} shell 的終端，鍵盤同步中` : undefined}
          onKeyDown={onTermKeyDown}
          onPaste={onTermPaste}
          onFocus={() => setTyping(true)}
          onBlur={() => setTyping(false)}
        >
          {body}
        </pre>
        {sync ? (
          <div className={`shell-sync-note${typing ? ' is-live' : ''}`} role="status">
            {typing
              ? '鍵盤同步中：按鍵直接送進這個 pane。⌘C／⌘R／⌘V 仍是瀏覽器的；Delete／Home／End／PgUp herdr 不收。'
              : '鍵盤同步開著，但焦點不在終端上——點一下上面的畫面才會收你的鍵盤。'}
          </div>
        ) : null}

        {readOnly ? (
          <div className="shell-sync-note" role="status">
            這是服務 pane（例如 dev server），只能看不能打字——送一個 Ctrl-C 就是把它關掉。
          </div>
        ) : null}

        <form
          className="shell-input"
          onSubmit={(e) => {
            e.preventDefault()
            if (!sending) void run(text)
          }}
        >
          <span className="shell-prompt mono" aria-hidden="true">
            ❯
          </span>
          <input
            ref={inputRef}
            type="text"
            className="shell-cmd mono"
            value={text}
            spellCheck={false}
            autoComplete="off"
            disabled={sync || readOnly}
            placeholder={
              readOnly ? '服務 pane 只能看' : sync ? '鍵盤同步中：直接在上面的終端打字' : '輸入指令，Enter 送出（↑↓ 翻歷史）'
            }
            aria-label={`對 ${host} 的 shell 輸入指令`}
            onChange={(e) => {
              setText(e.target.value)
              setHistAt(-1)
            }}
            onKeyDown={onKeyDown}
          />
          <button type="submit" className="btn primary" disabled={sending || sync || readOnly}>
            {sending ? '送出中…' : '送出'}
          </button>
        </form>

        {/* 唯讀時整列不渲染：`hidden` 會被 `.keypad{display:flex}` 蓋掉，按鈕照樣看得到也按得到。 */}
        {readOnly ? null : (
          <div className="keypad shell-keypad">
            {KEYS.map((k) => (
              <button key={k.label} type="button" className="key-btn" title={k.title} onClick={() => void pressKeys(k.keys)}>
                {k.label}
              </button>
            ))}
            {/* 清畫面 = 真的送 `clear`：前端清 state 下次輪詢就會抓回原內容。 */}
            <button type="button" className="key-btn" title="送出 clear，清掉終端畫面" onClick={() => void run('clear')}>
              清畫面
            </button>
            <span className="spacer" />
            <span className="hint shell-keypad-note">
              按鍵原樣送到那個 pane。↑↓ 在輸入框裡走的是這裡的指令歷史。
            </span>
          </div>
        )}
      </div>

      <ConfirmDialog
        open={confirmEnd}
        title="結束 shell"
        body={
          <>
            要關掉 <strong>{host === 'local' ? '本機' : host}</strong> 上這個 shell 嗎？
            它的 pane 會被關掉，裡面正在跑的指令也會跟著結束；畫面上的輸出不會留下來。
          </>
        }
        confirmLabel="結束 shell"
        danger
        onCancel={() => setConfirmEnd(false)}
        onConfirm={() => {
          setConfirmEnd(false)
          void endHostShell(host, paneId)
        }}
      />
    </>
  )
}
