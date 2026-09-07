import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { KeyboardEvent as ReactKeyboardEvent } from 'react'
import * as api from '../api'
import type { TerminalSnapshot, TerminalSource } from '../api/types'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { HostBadge } from './HostsPanel'
import { linkifyTerm } from './TermLinks'

/**
 * 對某台主機（本機或遠端）開著的那個 shell：終端快照 + 一行指令輸入。
 *
 * 存在的理由是「裝個 CLI、看一段 log、`gh auth status`、清掉一個 worktree」這些雜事不該
 * 需要另外開一個 terminal 再 ssh 一次。它**不是**一個 bot：沒有 run、沒有回合、沒有訊息
 * 紀錄，畫面上就只有那張快照與你打進去的字。
 *
 * 輪詢刻意寫在這裡而不是重用 `useTerminalSnapshot`：那支 hook 的參數是 `botId`，要它同時
 * 吃 bot 與 host 得改簽名，而 `ChatPanel` / `BlockedModal` 都在用。節奏（1 秒）、來源與
 * 「換目標就在 render 當下清畫面」都照它。
 */

/** 每台主機各記一份指令歷史。存在 localStorage：關掉面板再開回來還在。 */
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

/**
 * 還沒送出的那一行指令，依 `host/paneId` 各存一份。換 bot、關掉面板、重新整理都留著——
 * 打到一半的長指令不該因為切去看一眼對話就沒了。
 */
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

/**
 * 標題列只放 cwd 的最後兩段（`project/agents-manager`），完整路徑在 tooltip。
 * 倒數第二段另外包起來，手機寬度用 CSS 藏掉，只剩最後一段。
 */
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
  const hostUp = useStore((s) => (host === 'local' ? s.connected : (s.hosts.find((h) => h.name === host)?.connected ?? false)))
  const ending = useStore((s) => Boolean(s.busy[`shell:${host}:${paneId}`]))

  const [snap, setSnap] = useState<TerminalSnapshot | null>(null)
  const [err, setErr] = useState<string | null>(null)
  /** 改它就取消排著的那次、立刻重讀（送完指令要馬上看到反應）。 */
  const [nonce, setNonce] = useState(0)
  /** `visible` = 終端現在長什麼樣（shell 的常態）；`recent_unwrapped` = 連捲上去的一起看。 */
  const [source, setSource] = useState<TerminalSource>('visible')
  const [lines, setLines] = useState(200)
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
  const [confirmEnd, setConfirmEnd] = useState(false)
  const [history, setHistory] = useState<HistoryMap>(readHistory)
  /** 在歷史裡的位置；`-1` = 正在編輯的那一行（還沒往上翻）。 */
  const [histAt, setHistAt] = useState(-1)
  const inputRef = useRef<HTMLInputElement>(null)

  // 換 shell 時清畫面是 render 當下就該有的結果，不是一個 effect：留著上一個主機的終端內容
  // 不只是舊資料，是「另一台機器的畫面」，一眼看過去會以為是這一台的。
  const [lastTarget, setLastTarget] = useState(target)
  if (lastTarget !== target) {
    setLastTarget(target)
    setSnap(null)
    setErr(null)
    setTextState(readDrafts()[target] ?? '')
    setHistAt(-1)
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
        if (alive) setErr(e instanceof Error ? e.message : String(e))
      }
      if (alive) timer = setTimeout(() => void tick(), 1_000)
    }
    void tick()
    return () => {
      alive = false
      if (timer) clearTimeout(timer)
    }
  }, [host, paneId, source, lines, nonce])

  const refresh = useCallback(() => setNonce((n) => n + 1), [])

  // 這個面板存在的目的就是打字，所以一開就把焦點交給輸入框（換 shell 也一樣）。
  useEffect(() => {
    inputRef.current?.focus()
  }, [host, paneId])

  const remember = useCallback(
    (cmd: string) => {
      if (!cmd.trim()) return
      setHistory((h) => {
        const prev = h[host] ?? []
        // 同一個指令連按兩次不該在歷史裡佔兩格。
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
      // 走回 `-1` 就是回到「正在編輯的那一行」，而那一行本來是空的。
      setText(at < 0 ? '' : list[at])
    }
  }

  const body = useMemo(() => {
    if (!snap) return err ? `讀取終端失敗：${err}` : '讀取中…'
    // herdr 的 `recent_unwrapped` 只給「已經捲出畫面」的部分，還沒捲過的 pane 回的是空字串
    // （實測 2026-09-07：一個剛開的 shell 兩種 `recent` 都是空的，而 `visible` 有內容）。
    // 空白畫面看起來像壞掉，所以這裡把它說出來，而不是讓使用者以為讀不到。
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
      <button
        type="button"
        className="mini-btn danger"
        disabled={ending}
        onClick={() => setConfirmEnd(true)}
        title="關掉這個 shell 的 pane"
      >
        {ending ? '結束中…' : '結束 shell'}
      </button>
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
          <span className="hint term-pane-chip" title={`herdr pane ${paneId}`}>
            pane <code>{paneId}</code>
            {snap?.columns ? `・${snap.columns}×${snap.rows ?? '?'}` : ''}
          </span>
          {snap?.truncated ? <span className="hint">已截斷</span> : null}
          <span className="spacer" />
          <span className="hint term-bar-note">每秒更新，指令送出後立刻重讀</span>
          {embedded ? headActions : null}
        </div>

        {err && snap ? (
          <div className="shell-err" role="status">
            {/* 有快照卻讀失敗：畫面上那張是舊的，別讓它看起來像現況。 */}
            最後一次讀取失敗（{err}）——上面顯示的是先前的畫面。
          </div>
        ) : null}

        <pre className="term shell-term">{body}</pre>

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
            placeholder="輸入指令，Enter 送出（↑↓ 翻歷史）"
            aria-label={`對 ${host} 的 shell 輸入指令`}
            onChange={(e) => {
              setText(e.target.value)
              setHistAt(-1)
            }}
            onKeyDown={onKeyDown}
          />
          <button type="submit" className="btn primary" disabled={sending}>
            {sending ? '送出中…' : '送出'}
          </button>
        </form>

        <div className="keypad shell-keypad">
          {KEYS.map((k) => (
            <button key={k.label} type="button" className="key-btn" title={k.title} onClick={() => void pressKeys(k.keys)}>
              {k.label}
            </button>
          ))}
          {/* 清畫面 = 真的送 `clear`，不是前端把 state 清掉：使用者要的是終端乾淨，
              不是畫面假裝乾淨（下一次輪詢就會把原本的內容抓回來）。 */}
          <button type="button" className="key-btn" title="送出 clear，清掉終端畫面" onClick={() => void run('clear')}>
            清畫面
          </button>
          <span className="spacer" />
          <span className="hint shell-keypad-note">
            按鍵原樣送到那個 pane。↑↓ 在輸入框裡走的是這裡的指令歷史。
          </span>
        </div>
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
