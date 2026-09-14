import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { fetchMemProcesses, killMemProcess } from '../api'
import type { MemProcess, MemProcesses } from '../api/types'
import { LOCAL_HOST } from '../api/types'
import { useStore } from '../store/store'
import { humanBytes } from './MemBadge'
import { browsersLine, TABS_WARN, tabsTotal } from '../lib/browserMem'
import { MemPaneModal } from './MemPaneModal'
import './memPopover.css'

/**
 * 「RAM」點開的清單（SPEC §15.2）：依 `subtree_bytes` 降冪（砍掉能省多少）並列 owner。
 * bot 只給「停止 bot」（走 `POST /bots/{id}/stop`，直接送訊號會留下 daemon 以為還活著的 bot）；
 * 其餘兩段式 TERM → 再按才 KILL，KILL 會帶走沒存的東西所以要再確認。
 */

/** 15 秒 = daemon 的取樣間隔（SPEC §15.1）；比它快只會拿到同一份數字。 */
const REFRESH_MS = 15_000

function ownerLabel(p: MemProcess): string {
  if (p.owner === 'bot') return p.bot_name ?? `bot ${p.bot_id ?? ''}`.trim()
  if (p.owner === 'pane') return `自己開的 pane ${p.pane_id ?? ''}`.trim()
  if (p.owner === 'herdr') return 'herdr'
  return '?'
}

/** 一列的動作按鈕：bot 走「停止 bot」，其餘 TERM → 「強制」。 */
function RowAction({ p, host, onDone }: { p: MemProcess; host: string; onDone: () => void }) {
  const stopBot = useStore((s) => s.stopBot)
  const [busy, setBusy] = useState(false)
  const [armed, setArmed] = useState(false)
  const [err, setErr] = useState<string | null>(null)

  const run = useCallback(
    async (fn: () => Promise<void>) => {
      setBusy(true)
      setErr(null)
      try {
        await fn()
        onDone()
      } catch (e) {
        // daemon 的訊息照著顯示，不在前端另編一套。
        setErr(e instanceof Error ? e.message : '失敗')
      } finally {
        setBusy(false)
      }
    },
    [onDone],
  )

  if (p.owner === 'bot') {
    const id = p.bot_id
    return (
      <div className="mem-pop-act">
        <button type="button" disabled={busy || !id} onClick={() => id && run(() => stopBot(id))} title="走正規的停止流程，daemon 會收掉這個 run">
          停止 bot
        </button>
        {err ? <span className="mem-pop-err">{err}</span> : null}
      </div>
    )
  }

  return (
    <div className="mem-pop-act">
      <button
        type="button"
        className={armed ? 'danger' : ''}
        disabled={busy}
        title={armed ? 'SIGKILL：立刻結束，還沒存的東西會一起沒了' : 'SIGTERM：讓它自己收尾'}
        onClick={() =>
          void run(async () => {
            await killMemProcess(host, p.pid, armed ? 'KILL' : 'TERM')
            // TERM 後按鈕換成「強制」：程序還在就是沒理 TERM。
            setArmed(!armed)
          })
        }
      >
        {armed ? '強制' : '結束'}
      </button>
      {err ? <span className="mem-pop-err">{err}</span> : null}
    </div>
  )
}

/** 點得開的 RAM 徽章。 */
export function MemPopover({ host = LOCAL_HOST, children }: { host?: string; children: React.ReactNode }) {
  const [open, setOpen] = useState(false)
  const [data, setData] = useState<MemProcesses | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const wrap = useRef<HTMLDivElement>(null)
  const btnRef = useRef<HTMLButtonElement>(null)
  const popRef = useRef<HTMLDivElement>(null)
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)
  const row = useStore((s) => s.mem?.hosts.find((h) => h.host === host) ?? null)

  const load = useCallback(async () => {
    setLoading(true)
    try {
      setData(await fetchMemProcesses(host))
      setErr(null)
    } catch (e) {
      setErr(e instanceof Error ? e.message : '讀不到')
    } finally {
      setLoading(false)
    }
  }, [host])

  // 關著就不抓：每次是一趟 `ps`（遠端還要 ssh）；第一筆由點開的 `onClick` 抓。
  useEffect(() => {
    if (!open) return
    const t = setInterval(() => void load(), REFRESH_MS)
    return () => clearInterval(t)
  }, [open, load])

  useLayoutEffect(() => {
    if (!open) {
      setPos(null)
      return
    }
    const place = () => {
      const el = btnRef.current
      const pop = popRef.current
      if (!el) return
      const r = el.getBoundingClientRect()
      const margin = 8
      const w = pop?.offsetWidth || Math.min(560, window.innerWidth - 24)
      const h = pop?.offsetHeight || 280
      let left = r.left
      if (left + w + margin > window.innerWidth) left = window.innerWidth - w - margin
      left = Math.max(margin, left)
      let top = r.bottom + 6
      if (top + h + margin > window.innerHeight) top = Math.max(margin, r.top - 6 - h)
      setPos({ left, top })
    }
    place()
    window.addEventListener('resize', place)
    window.addEventListener('scroll', place, true)
    return () => {
      window.removeEventListener('resize', place)
      window.removeEventListener('scroll', place, true)
    }
  }, [open, data, loading])

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      const t = e.target as Node
      // pane 視窗是 portal 到 body 的，不在 `wrap` 裡；點它不算「點到外面」。
      if ((t as Element).closest?.('.mem-pane-backdrop')) return
      if (btnRef.current?.contains(t) || popRef.current?.contains(t)) return
      setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDoc)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  const title = host === LOCAL_HOST ? '本機' : host
  const rows = data?.processes ?? []
  // 點「自己開的 pane」開的那個視窗（`MemPaneModal`）；清單留在後面不關。
  const [peek, setPeek] = useState<MemProcess | null>(null)

  const pop = open ? (
        <div
          ref={popRef}
          className="mem-pop"
          role="dialog"
          aria-label={`${title} 的記憶體明細`}
          style={pos ? { left: pos.left, top: pos.top } : { visibility: 'hidden', left: 0, top: 0 }}
        >
          <div className="mem-pop-head">
            <span className="mem-pop-title">
              {title} RAM {humanBytes(row?.total_bytes ?? 0)} · herdr {humanBytes(row?.herdr_bytes ?? 0)} · {row?.processes ?? 0} 個 process
            </span>
            <button type="button" className="mem-pop-refresh" disabled={loading} onClick={() => void load()}>
              重新整理
            </button>
          </div>
          {row?.machine ? (
            <p className={`mem-pop-machine${row.machine.available_bytes / row.machine.total_bytes < 0.15 ? ' low' : ''}`}>
              這台機器 剩 <strong>{humanBytes(row.machine.available_bytes)}</strong> / 共 {humanBytes(row.machine.total_bytes)}
              <span className="mem-pop-machine-used">
                （已用 {humanBytes(row.machine.total_bytes - row.machine.available_bytes)}，其中 herdr 樹 {humanBytes(row.total_bytes)}）
              </span>
            </p>
          ) : null}
          {row?.browsers.length ? (
            <p className={`mem-pop-browsers${tabsTotal(row.browsers) >= TABS_WARN ? ' hot' : ''}`}>
              瀏覽器 {browsersLine(row.browsers)}
              {tabsTotal(row.browsers) >= TABS_WARN ? `——超過 ${TABS_WARN} 個分頁，RAM 多半是它們吃的，關一些。` : '（不算在上面的 RAM 裡）'}
            </p>
          ) : null}
          {err ? <p className="mem-pop-err">{err}</p> : null}
          {!err && rows.length === 0 ? (
            <p className="mem-pop-empty">{loading ? '取樣中…' : '沒有程序。'}</p>
          ) : null}
          {rows.length > 0 ? (
            <table className="mem-pop-table">
              <thead>
                <tr>
                  <th>大小</th>
                  <th>程式</th>
                  <th>誰的</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {rows.map((p) => {
                  const canPeek = p.owner !== 'bot' && !!p.pane_id
                  return (
                    <tr key={p.pid} className={p.owner === 'bot' ? 'is-bot' : ''}>
                      <td className="mem-pop-size" title={`自己 ${humanBytes(p.rss_bytes)}，連同 ${p.children} 個子程序`}>
                        {humanBytes(p.subtree_bytes)}
                      </td>
                      <td className="mem-pop-argv" title={p.argv}>
                        {p.argv}
                      </td>
                      <td className="mem-pop-owner">
                        {canPeek ? (
                          <button type="button" className="mem-pop-peek" title="開一個視窗看這個 pane 現在的畫面" onClick={() => setPeek(p)}>
                            {ownerLabel(p)}
                            <span className="mem-pop-caret" aria-hidden="true">↗</span>
                          </button>
                        ) : (
                          ownerLabel(p)
                        )}
                      </td>
                      <td>
                        <RowAction p={p} host={host} onDone={() => void load()} />
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          ) : null}
          <p className="mem-pop-foot">結束的是那個 pane 裡的程序，pane 本身還在。</p>
        </div>
  ) : null

  return (
    <div className="mem-wrap" ref={wrap}>
      <button
        ref={btnRef}
        type="button"
        className="mem-open"
        aria-expanded={open}
        aria-haspopup="dialog"
        aria-label={`${title} 的記憶體明細`}
        onClick={() => {
          setOpen((v) => !v)
          if (!open) void load()
        }}
      >
        {children}
      </button>
      {pop ? createPortal(pop, document.body) : null}
      {open && peek ? <MemPaneModal host={host} p={peek} onClose={() => setPeek(null)} /> : null}
    </div>
  )
}
