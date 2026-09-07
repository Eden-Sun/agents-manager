import { useCallback, useEffect, useRef, useState } from 'react'
import { fetchMemProcesses, killMemProcess } from '../api'
import type { MemProcess, MemProcesses } from '../api/types'
import { LOCAL_HOST } from '../api/types'
import { useStore } from '../store/store'
import { humanBytes } from './MemBadge'

/**
 * 「RAM 4.5G」點開之後的那張清單（SPEC §15.2）。
 *
 * 一個總數回答不了使用者真正的問題：**這裡面哪些是我自己開的、可以砍掉**。一台機器底下
 * 十幾個 `claude`，一半是手動開的 pane 或舊的 `--resume`。所以清單依 `subtree_bytes`
 * 降冪——那正是「砍掉這個能省多少」——並把 owner 直接寫在列上。
 *
 * bot 不給 kill，給的是「停止 bot」：走 `POST /bots/{id}/stop` 那條路才會記錄停止、收掉
 * run，直接送訊號只會留下一個 daemon 以為還活著的 bot。
 *
 * 結束是兩段式（TERM → 再按一次才 KILL）。TERM 讓 CLI 有機會把 session 寫完；第一次按下去
 * 通常就夠了，KILL 要使用者再確認一次，因為它會把還沒存的東西一起帶走。
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
        // daemon 的訊息照著顯示（「這是 AG Man 的 bot，請用停止 bot」等），不在前端另編一套。
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
            // TERM 送出去之後就把按鈕換成「強制」：程序死了這一列會消失，還在就是它沒理
            // TERM，那時使用者要的正是下一段。KILL 之後沒有下一段了。
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

/** 點得開的 RAM 徽章：外觀跟原本那顆一樣，差在它是 `button` 且會掛清單。 */
export function MemPopover({ host = LOCAL_HOST, children }: { host?: string; children: React.ReactNode }) {
  const [open, setOpen] = useState(false)
  const [data, setData] = useState<MemProcesses | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const wrap = useRef<HTMLDivElement>(null)
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

  // 關著就不抓：這是一次 `ps`（遠端還是一趟 ssh），沒人在看的時候跑它只是白花錢。
  // 第一筆由「點開」那個動作去抓（見下面的 `onClick`），這裡只負責之後的重取樣。
  useEffect(() => {
    if (!open) return
    const t = setInterval(() => void load(), REFRESH_MS)
    return () => clearInterval(t)
  }, [open, load])

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (wrap.current && !wrap.current.contains(e.target as Node)) setOpen(false)
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

  return (
    <div className="mem-wrap" ref={wrap}>
      <button type="button" className="mem-open" aria-expanded={open} aria-haspopup="dialog" aria-label={`${title} 的記憶體明細`} onClick={() => {
          setOpen((v) => !v)
          if (!open) void load()
        }}
      >
        {children}
      </button>
      {open ? (
        <div className="mem-pop" role="dialog" aria-label={`${title} 的記憶體明細`}>
          <div className="mem-pop-head">
            <span className="mem-pop-title">
              {title} RAM {humanBytes(row?.total_bytes ?? 0)} · herdr {humanBytes(row?.herdr_bytes ?? 0)} · {row?.processes ?? 0} 個 process
            </span>
            <button type="button" className="mem-pop-refresh" disabled={loading} onClick={() => void load()}>
              重新整理
            </button>
          </div>
          {err ? <p className="mem-pop-err">{err}</p> : null}
          {!err && rows.length === 0 ? (
            <p className="mem-pop-empty">{loading ? '取樣中…' : '這台 daemon 還不會列程序（需要重啟成新版）。'}</p>
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
                {rows.map((p) => (
                  <tr key={p.pid} className={p.owner === 'bot' ? 'is-bot' : ''}>
                    <td className="mem-pop-size" title={`自己 ${humanBytes(p.rss_bytes)}，連同 ${p.children} 個子程序`}>
                      {humanBytes(p.subtree_bytes)}
                    </td>
                    <td className="mem-pop-argv" title={p.argv}>
                      {p.argv}
                    </td>
                    <td className="mem-pop-owner">{ownerLabel(p)}</td>
                    <td>
                      <RowAction p={p} host={host} onDone={() => void load()} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          ) : null}
          <p className="mem-pop-foot">結束的是那個 pane 裡的程序，pane 本身還在。</p>
        </div>
      ) : null}
    </div>
  )
}
