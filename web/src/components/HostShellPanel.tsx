import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { ClipboardEvent as ReactClipboardEvent, KeyboardEvent as ReactKeyboardEvent } from 'react'
import * as api from '../api'
import { ClaudeLoginAssist } from './ClaudeLoginAssist'
import { herdrKeyFromEvent, useShellKeys } from '../hooks/usePaneKeys'
import { initialKeySync, keySyncActive, shellForbidden, shellStateUnknown } from '../lib/shellAccess'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { isImeEnter } from '../lib/ime'
import { ApiError } from '../api/types'
import type { TerminalSnapshot, TerminalSource } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { shellDraftKey } from '../store/shellDraft'
import { ConfirmDialog } from './ConfirmDialog'
import { ServicePaneNote } from './ServicePaneNote'
import { HostShellXterm } from './HostShellXterm'
import { HostBadge } from './HostsPanel'
import { linkifyTerm } from './termLinks'
import { fitRules } from '../lib/termRules'
import { splitAtCursor } from '../lib/termCursor'
import { setTermWrap, useTermWrap } from './termWrap'
import { isHeldLoginPane } from '../lib/loginPaneHold'
import { useLoginShellAutoClose } from './useLoginShellAutoClose'
import { readFitScreen, writeFitScreen } from './shellFitScreen'
import './hostShellPanel.css'

const SYNC_OFF_KEY = 'am.shellKeySyncOff'
const SYNC_ON_KEY = 'am.shellKeySyncOn'
const SYNC_POLL_MS = 250
const IDLE_POLL_MS = 1_000

function readSyncOffSet(): Set<string> {
  return readTargetSet(SYNC_OFF_KEY)
}

function readSyncOnSet(): Set<string> {
  return readTargetSet(SYNC_ON_KEY)
}

function readTargetSet(key: string): Set<string> {
  try {
    const raw = localStorage.getItem(key)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    return new Set(Array.isArray(parsed) ? parsed.filter((x): x is string => typeof x === 'string') : [])
  } catch {
    return new Set()
  }
}

function writeSync(target: string, on: boolean) {
  try {
    const off = readSyncOffSet()
    const onSet = readSyncOnSet()
    if (on) {
      off.delete(target)
      onSet.add(target)
    } else {
      off.add(target)
      onSet.delete(target)
    }
    localStorage.setItem(SYNC_OFF_KEY, JSON.stringify([...off]))
    localStorage.setItem(SYNC_ON_KEY, JSON.stringify([...onSet]))
  } catch {
  }
}

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
  }
}

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
  embedded?: boolean
}) {
  const t = useT()
  const endHostShell = useStore((s) => s.endHostShell)
  const readOnly = useStore((s) => Boolean(s.shellView?.host === host && s.shellView.paneId === paneId && s.shellView.readOnly))
  const readOnlyReason = useStore((s) =>
    s.shellView?.host === host && s.shellView.paneId === paneId ? (s.shellView.readOnlyReason ?? null) : null,
  )
  const lockShellView = useStore((s) => s.lockShellView)
  const paneGone = useStore((s) => s.paneGone)
  const traced = useStore((s) => Boolean(s.shellView?.host === host && s.shellView.paneId === paneId && s.shellView.traced))
  const hostUp = useStore((s) => (host === 'local' ? s.connected : (s.hosts.find((h) => h.name === host)?.connected ?? false)))
  const ending = useStore((s) => Boolean(s.busy[`shell:${host}:${paneId}`]))

  const [snap, setSnap] = useState<TerminalSnapshot | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const [sendErr, setSendErr] = useState<string | null>(null)
  const [nonce, setNonce] = useState(0)
  const [source, setSource] = useState<TerminalSource>('visible')
  const [lines, setLines] = useState(200)
  const wrap = useTermWrap()
  useLoginShellAutoClose(host, paneId)
  const target = `${host}/${paneId}`
  const draftKey = shellDraftKey(host, paneId)
  const text = useStore((s) => s.drafts[draftKey] ?? '')
  const setDraft = useStore((s) => s.setDraft)
  const setText = useCallback((v: string) => setDraft(draftKey, v), [setDraft, draftKey])
  const [sending, setSending] = useState(false)
  const phone = useMediaQuery(PHONE_QUERY)
  const initialSync = () => initialKeySync(target, { phone, off: readSyncOffSet(), on: readSyncOnSet() })
  const [sync, setSyncState] = useState(initialSync)
  const syncOn = keySyncActive(sync, readOnly)
  const [typing, setTyping] = useState(false)
  const xtermMode = syncOn && !phone && !readOnly && source === 'visible'
  const [xtermGeo, setXtermGeo] = useState<{ cols: number; rows: number } | null>(null)
  const [xtermStreaming, setXtermStreaming] = useState(false)
  const [xtermObserve, setXtermObserve] = useState(false)
  const [fitScreen, setFitScreenState] = useState(readFitScreen)
  const setFitScreen = (on: boolean) => {
    setFitScreenState(on)
    writeFitScreen(on)
  }
  const termRef = useRef<HTMLPreElement>(null)
  const [confirmEnd, setConfirmEnd] = useState(false)
  const [endNeedsConfirm, setEndNeedsConfirm] = useState<api.CloseNeedsConfirm | null>(null)
  const [history, setHistory] = useState<HistoryMap>(readHistory)
  const [histAt, setHistAt] = useState(-1)
  const inputRef = useRef<HTMLInputElement>(null)
  const pendingLine = useRef('')

  const [lastTarget, setLastTarget] = useState(target)
  if (lastTarget !== target) {
    setLastTarget(target)
    setSnap(null)
    setErr(null)
    setSendErr(null)
    setHistAt(-1)
    setSyncState(initialSync())
    setTyping(false)
    setXtermGeo(null)
    setXtermStreaming(false)
    setXtermObserve(false)
  }

  useEffect(() => {
    if (xtermMode) return
    let alive = true
    let busy = false
    let timer: ReturnType<typeof setTimeout> | null = null
    const next = () => {
      if (alive) timer = setTimeout(() => void tick(), syncOn ? SYNC_POLL_MS : IDLE_POLL_MS)
    }
    const tick = async () => {
      if (document.visibilityState === 'hidden') {
        next()
        return
      }
      busy = true
      try {
        const s = await api.readHostShell(host, paneId, source, lines)
        if (alive) {
          setSnap(s)
          setErr(null)
        }
      } catch (e) {
        if (e instanceof ApiError && e.status === 404) {
          busy = false
          if (alive && !isHeldLoginPane(host, paneId)) paneGone(host, paneId)
          return
        }
        const denied = shellForbidden(e)
        if (denied) lockShellView(host, paneId, denied)
        if (alive) setErr(denied ?? (e instanceof Error ? e.message : String(e)))
      }
      busy = false
      next()
    }
    const onVisible = () => {
      if (document.visibilityState !== 'visible' || busy) return
      if (timer) clearTimeout(timer)
      timer = null
      void tick()
    }
    document.addEventListener('visibilitychange', onVisible)
    void tick()
    return () => {
      alive = false
      document.removeEventListener('visibilitychange', onVisible)
      if (timer) clearTimeout(timer)
    }
  }, [paneGone, lockShellView, host, paneId, source, lines, nonce, syncOn, xtermMode])

  const refresh = useCallback(() => setNonce((n) => n + 1), [])

  const xtermRead = useCallback(
    (e: unknown | null) => {
      if (e === null) {
        setErr(null)
        return
      }
      if (e instanceof ApiError && e.status === 404) {
        if (!isHeldLoginPane(host, paneId)) paneGone(host, paneId)
        return
      }
      const denied = shellForbidden(e)
      if (denied) lockShellView(host, paneId, denied)
      setErr(denied ?? (e instanceof Error ? e.message : String(e)))
    },
    [paneGone, lockShellView, host, paneId],
  )

  useEffect(() => {
    if (syncOn) termRef.current?.focus()
    else inputRef.current?.focus()
  }, [host, paneId, syncOn])

  const failed = useCallback(
    (e: unknown) => {
      const denied = shellForbidden(e)
      if (!denied) {
        setSendErr(shellStateUnknown(e) ?? (e instanceof Error ? e.message : String(e)))
        return
      }
      lockShellView(host, paneId, denied)
      setSyncState(false)
      writeSync(target, false)
      setSendErr(null)
      setErr(null)
    },
    [host, lockShellView, paneId, target],
  )

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
        if ((useStore.getState().drafts[draftKey] ?? '') === cmd) {
          setText('')
          setHistAt(-1)
          pendingLine.current = ''
        }
        setSendErr(null)
        refresh()
      } catch (e) {
        failed(e)
      } finally {
        setSending(false)
      }
    },
    [draftKey, failed, host, paneId, refresh, remember, setText],
  )

  const { press: pressSync, paste: pasteSync } = useShellKeys(
    host,
    paneId,
    useCallback(
      (e: unknown | null) => {
        if (e) failed(e)
        else setSendErr(null)
        refresh()
      },
      [failed, refresh],
    ),
  )

  const xtermSend = useCallback(
    (e: unknown | null) => {
      if (e) failed(e)
      else setSendErr(null)
    },
    [failed],
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

  const onTermKeyDown = (e: ReactKeyboardEvent<HTMLPreElement>) => {
    if (!syncOn) return
    const key = herdrKeyFromEvent(e.nativeEvent)
    if (!key) return
    e.preventDefault()
    pressSync([key])
  }

  const onTermPaste = (e: ReactClipboardEvent<HTMLPreElement>) => {
    if (!syncOn) return
    const text = e.clipboardData.getData('text')
    if (!text) return
    e.preventDefault()
    pasteSync(text)
  }

  const onKeyDown = (e: ReactKeyboardEvent<HTMLInputElement>) => {
    if (isImeEnter(e.nativeEvent)) return
    if (e.key === 'Enter') {
      e.preventDefault()
      if (!sending) void run(text)
      return
    }
    const list = history[host] ?? []
    if (e.key === 'ArrowUp' && list.length > 0) {
      e.preventDefault()
      if (histAt === -1) pendingLine.current = text
      const at = Math.min(histAt + 1, list.length - 1)
      setHistAt(at)
      setText(list[at])
      return
    }
    if (e.key === 'ArrowDown' && histAt >= 0) {
      e.preventDefault()
      const at = histAt - 1
      setHistAt(at)
      setText(at < 0 ? pendingLine.current : list[at])
    }
  }

  const body = useMemo(() => {
    if (!snap) return err ? t('讀取終端失敗：{error}', { error: err }) : t('讀取中…')
    if (!snap.text.trim() && source === 'recent_unwrapped') {
      return t('（還沒有捲出畫面的內容——這個 shell 的輸出目前都還在「畫面」裡。）')
    }
    if (syncOn && !readOnly && source === 'visible') {
      const { before, gap, after } = splitAtCursor(snap.text)
      return (
        <>
          {linkifyTerm(before, snap.columns)}
          {gap}
          <span className={`term-cursor${typing ? ' is-live' : ''}`} aria-hidden="true" />
          {fitRules(after)}
        </>
      )
    }
    return linkifyTerm(snap.text, snap.columns)
  }, [err, snap, source, syncOn, readOnly, typing, t])

  const headActions = (
    <div className="head-actions">
      {}
      <button
        type="button"
        className="mini-btn danger"
        disabled={ending}
        onClick={() => setConfirmEnd(true)}
        title={traced ? t('關掉這顆 pane（裡面跑的東西會一起結束）') : t('關掉這個 shell 的 pane')}
      >
        {ending ? t('關閉中…') : traced ? t('關閉 pane') : t('結束 shell')}
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
          aria-label={t('開啟側邊欄')}
          data-tip={t('開啟側邊欄')}
        >
          ☰
        </button>
        <div className="main-title">
          <span className="shell-icon" aria-hidden="true">
            ❯
          </span>
          <strong>{host === 'local' ? t('本機 shell') : t('{host} shell', { host })}</strong>
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
        <ClaudeLoginAssist host={host} paneId={paneId} />
        <div className="term-bar shell-bar">
          {embedded ? (
            <span className="hint" title={cwd}>
              <strong>{host === 'local' ? t('本機 shell') : t('{host} shell', { host })}</strong>
              <HostBadge host={host} connected={hostUp} />
              <span className="shell-cwd mono">{cwdTail(cwd).leaf}</span>
            </span>
          ) : null}
          <label className="conn">
            {t('檢視')}
            <select value={source} onChange={(e) => setSource(e.target.value as TerminalSource)}>
              <option value="visible">{t('畫面')}</option>
              <option value="recent_unwrapped">{t('含捲動歷史')}</option>
            </select>
          </label>
          {source === 'recent_unwrapped' ? (
            <label className="conn">
              {t('行數')}
              <select value={lines} onChange={(e) => setLines(Number(e.target.value))}>
                {[100, 200, 500, 1000].map((n) => (
                  <option key={n} value={n}>
                    {n}
                  </option>
                ))}
              </select>
            </label>
          ) : null}
          <button type="button" className="mini-btn" onClick={refresh} title={t('立刻重讀一次（平常每秒自己更新）')}>
            {t('刷新')}
          </button>
          <button
            type="button"
            className={`mini-btn${syncOn ? ' on' : ''}`}
            aria-pressed={syncOn}
            disabled={readOnly}
            onClick={() => setSync(!syncOn)}
            title={
              syncOn
                ? t('關掉鍵盤直通，改用下方輸入框「打一行、Enter 送出」')
                : t('打開鍵盤直通：每一下按鍵直接送進這個 pane（⌘ 系列留給瀏覽器）')
            }
          >
            {syncOn ? t('鍵盤直通') : t('鍵盤直通（關）')}
          </button>
          {}
          {xtermMode && xtermObserve ? null : (
          <label className="conn" title={t('折行後 TUI 畫的框線與對齊會跑掉，但整行讀得到；不折行則維持原樣，靠橫捲看右半邊。')}>
            <input type="checkbox" checked={wrap} onChange={(e) => setTermWrap(e.target.checked)} />
            {t('換行')}
          </label>
          )}
          {xtermMode && !xtermObserve ? (
            <label className="conn" title={t('把整個 pane 縮進面板（字會變小）；關掉時字不小於 12px，放不下就捲動。')}>
              <input type="checkbox" checked={fitScreen} onChange={(e) => setFitScreen(e.target.checked)} />
              {t('縮放至全螢幕')}
            </label>
          ) : null}
          <span className="hint term-pane-chip" title={`herdr pane ${paneId}`}>
            pane <code>{paneId}</code>
            {xtermMode && xtermGeo
              ? t('・{cols}×{rows}', { cols: xtermGeo.cols, rows: xtermGeo.rows })
              : snap?.columns
                ? t('・{cols}×{rows}', { cols: snap.columns, rows: snap.rows ?? '?' })
                : ''}
          </span>
          {snap?.truncated ? <span className="hint">{t('已截斷')}</span> : null}
          <span className="spacer" />
          <span className="hint term-bar-note">
            {syncOn
              ? xtermMode && xtermStreaming
                ? t('鍵盤直通中，推送更新')
                : t('鍵盤直通中，每 0.25 秒更新')
              : t('每秒更新，指令送出後立刻重讀')}
          </span>
          {embedded ? headActions : null}
        </div>

        {err && snap ? (
          <div className="shell-err" role="status">
            {}
            {t('最後一次讀取失敗（{error}）——上面顯示的是先前的畫面。', { error: err })}
          </div>
        ) : null}
        {sendErr ? (
          <div className="shell-err" role="alert">
            {t('送出失敗（{error}）——這次的輸入沒有送進 pane。', { error: sendErr })}
          </div>
        ) : null}

        {xtermMode ? (
          <HostShellXterm
            key={`${host}/${paneId}`}
            host={host}
            paneId={paneId}
            wrap={wrap}
            onTyping={setTyping}
            onRead={xtermRead}
            onSend={xtermSend}
            onGeometry={(cols, rows) => setXtermGeo({ cols, rows })}
            onStreaming={setXtermStreaming}
            onObserve={setXtermObserve}
            fitScreen={fitScreen}
          />
        ) : (
        <pre
          ref={termRef}
          className={`term shell-term${wrap ? ' term-wrap' : ''}${syncOn ? ' term-sync' : ''}${syncOn && typing ? ' term-sync-live' : ''}`}
          tabIndex={syncOn ? 0 : -1}
          role={syncOn ? 'textbox' : undefined}
          aria-label={syncOn ? t('{host} shell 的終端，鍵盤直通中', { host: host === 'local' ? t('本機') : host }) : undefined}
          onKeyDown={onTermKeyDown}
          onPaste={onTermPaste}
          onFocus={() => setTyping(true)}
          onBlur={() => setTyping(false)}
        >
          {body}
        </pre>
        )}
        {syncOn ? (
          <div className={`shell-sync-note${typing ? ' is-live' : ''}`} role="status">
            {typing
              ? t('鍵盤直通中：按鍵直接送進這個 pane。⌘C／⌘R／⌘V 仍是瀏覽器的。')
              : phone
                ? t('手機沒有實體鍵盤：關掉鍵盤直通改用下方輸入框。')
                : t('鍵盤直通開著，但焦點不在終端上——點一下上面的畫面才會收你的鍵盤。')}
          </div>
        ) : null}

        {readOnly ? (
          <div className="shell-sync-note" role="status">
            {readOnlyReason ?? t('這顆 pane 開著 port（例如 dev server），只能看不能打字——送一個 Ctrl-C 就是把它關掉。')}
          </div>
        ) : null}

        {}
        {syncOn ? null : (
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
            disabled={syncOn || readOnly}
            placeholder={readOnly ? t('這顆 pane 只能看') : t('輸入指令，Enter 送出（↑↓ 翻歷史）')}
            aria-label={t('對 {host} 的 shell 輸入指令', { host })}
            onChange={(e) => {
              setText(e.target.value)
              setHistAt(-1)
            }}
            onKeyDown={onKeyDown}
          />
          <button type="submit" className="btn primary" disabled={sending || syncOn || readOnly}>
            {sending ? t('送出中…') : t('送出')}
          </button>
        </form>
        )}
      </div>

      <ConfirmDialog
        open={confirmEnd}
        title={traced ? t('關閉 pane') : t('結束 shell')}
        body={
          <>
            {t.x('要關掉 {host} 上的 {target} 嗎？它的 pane 會被關掉，裡面正在跑的指令也會跟著結束；畫面上的輸出不會留下來。', {
              host: <strong>{host === 'local' ? t('本機') : host}</strong>,
              target: traced ? t.x('pane {id}', { id: <code>{paneId}</code> }) : t('這個 shell'),
            })}
          </>
        }
        confirmLabel={traced ? t('關閉 pane') : t('結束 shell')}
        danger
        onCancel={() => setConfirmEnd(false)}
        onConfirm={() => {
          setConfirmEnd(false)
          void endHostShell(host, paneId).then((needs) => {
            if (needs) setEndNeedsConfirm(needs)
          })
        }}
      />

      {}
      <ConfirmDialog
        open={endNeedsConfirm !== null}
        title={t('這顆 pane 還在做事')}
        body={<ServicePaneNote needs={endNeedsConfirm} />}
        confirmLabel={t('仍要關掉')}
        danger
        onCancel={() => setEndNeedsConfirm(null)}
        onConfirm={() => {
          setEndNeedsConfirm(null)
          void endHostShell(host, paneId, true)
        }}
      />
    </>
  )
}
