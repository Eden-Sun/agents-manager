import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { fetchMemProcesses, killMemProcess } from '../api'
import type { MemProcess, MemProcesses } from '../api/types'
import { LOCAL_HOST } from '../api/types'
import { useStore } from '../store/store'
import { humanBytes } from './memFormat'
import { browsersLine, TABS_WARN, tabsTotal } from '../lib/browserMem'
import { MemPaneModal } from './MemPaneModal'
import { useT, type TFn } from '../lib/i18n'
import './memPopover.css'

const REFRESH_MS = 15_000
const LOAD_TIMEOUT_MS = 15_000

const CHILD_EXES = new Set(['bash', 'sh', 'zsh', 'fish', 'node'])

function isChildProcess(p: MemProcess): boolean {
  if (p.owner !== 'bot') return false
  const exe = p.exe?.toLowerCase()
  return Boolean(exe && CHILD_EXES.has(exe))
}

function ownerLabel(p: MemProcess, t: TFn): string {
  if (p.owner === 'bot') {
    const name = p.bot_name ?? `bot ${p.bot_id ?? ''}`.trim()
    if (isChildProcess(p)) {
      return t('{name}（子程序）', { name })
    }
    return name
  }
  if (p.owner === 'pane') return t('自己開的 pane {id}', { id: p.pane_id ?? '' }).trim()
  if (p.owner === 'herdr') return 'herdr'
  return '?'
}

export function RowAction({ p, host, onDone }: { p: MemProcess; host: string; onDone: () => void }) {
  const t = useT()
  const stopBot = useStore((s) => s.stopBot)
  const [busy, setBusy] = useState(false)
  const [armed, setArmed] = useState(false)
  const [stopArmed, setStopArmed] = useState(false)
  useEffect(() => {
    if (!stopArmed) return
    const timer = setTimeout(() => setStopArmed(false), 3000)
    return () => clearTimeout(timer)
  }, [stopArmed])
  const [err, setErr] = useState<string | null>(null)

  const run = useCallback(
    async (fn: () => Promise<void>) => {
      setBusy(true)
      setErr(null)
      try {
        await fn()
        onDone()
      } catch (e) {
        setErr(e instanceof Error ? e.message : t('失敗'))
      } finally {
        setBusy(false)
      }
    },
    [onDone, t],
  )

  if (p.owner === 'bot') {
    const id = p.bot_id
    if (isChildProcess(p)) {
      return null
    }
    return (
      <div className="mem-pop-act">
        {}
        <button
          type="button"
          className={stopArmed ? 'danger' : ''}
          disabled={busy || !id}
          onClick={() => {
            if (!id) return
            if (!stopArmed) {
              setStopArmed(true)
              return
            }
            setStopArmed(false)
            void run(() => stopBot(id))
          }}
          title={stopArmed ? t('再按一次才真的停止（會中斷它正在跑的回合）') : t('走正規的停止流程，daemon 會收掉這個 run；要按兩下確認')}
        >
          {stopArmed ? t('再按一次確定停止') : t('停止 bot')}
        </button>
        {err ? <span className="mem-pop-err" role="alert">{err}</span> : null}
      </div>
    )
  }

  return (
    <div className="mem-pop-act">
      <button
        type="button"
        className={armed ? 'danger' : ''}
        disabled={busy}
        title={armed ? t('SIGKILL：立刻結束，還沒存的東西會一起沒了') : t('SIGTERM：讓它自己收尾')}
        onClick={() =>
          void run(async () => {
            await killMemProcess(host, p.pid, armed ? 'KILL' : 'TERM')
            setArmed(!armed)
          })
        }
      >
        {armed ? t('強制') : t('結束')}
      </button>
      {err ? <span className="mem-pop-err" role="alert">{err}</span> : null}
    </div>
  )
}

export function MemPopover({ host: initialHost = LOCAL_HOST, hosts, children }: { host?: string; hosts?: string[]; children: React.ReactNode }) {
  const t = useT()
  const [open, setOpen] = useState(false)
  const [host, setHost] = useState(initialHost)
  const [data, setData] = useState<MemProcesses | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const wrap = useRef<HTMLDivElement>(null)
  const btnRef = useRef<HTMLButtonElement>(null)
  const popRef = useRef<HTMLDivElement>(null)
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)
  const row = useStore((s) => s.mem?.hosts.find((h) => h.host === host) ?? null)

  const gen = useRef(0)
  const inflight = useRef(0)
  const load = useCallback(async (target: string = host) => {
    const mine = ++gen.current
    inflight.current += 1
    const controller = new AbortController()
    const timeout = setTimeout(() => controller.abort(new DOMException(t('記憶體行程清單讀取逾時'), 'TimeoutError')), LOAD_TIMEOUT_MS)
    setLoading(true)
    try {
      const d = await fetchMemProcesses(target, controller.signal)
      if (mine !== gen.current) return
      setData(d)
      setErr(null)
    } catch (e) {
      if (mine === gen.current) setErr(e instanceof Error ? e.message : t('讀不到'))
    } finally {
      clearTimeout(timeout)
      inflight.current -= 1
      if (mine === gen.current) setLoading(false)
    }
  }, [host, t])

  useEffect(() => {
    if (!open) return
    let alive = true
    let timer: ReturnType<typeof setTimeout> | null = null
    const next = () => {
      if (alive) timer = setTimeout(() => void tick(), REFRESH_MS)
    }
    const tick = async () => {
      if (document.visibilityState === 'hidden' || inflight.current > 0) return next()
      await load()
      next()
    }
    const onVisible = () => {
      if (document.visibilityState !== 'visible' || inflight.current > 0) return
      if (timer) clearTimeout(timer)
      timer = null
      void tick()
    }
    document.addEventListener('visibilitychange', onVisible)
    next()
    return () => {
      alive = false
      document.removeEventListener('visibilitychange', onVisible)
      if (timer) clearTimeout(timer)
    }
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
      const trgt = e.target as Node
      if ((trgt as Element).closest?.('.mem-pane-backdrop')) return
      if (btnRef.current?.contains(trgt) || popRef.current?.contains(trgt)) return
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

  const title = host === LOCAL_HOST ? t('本機') : host
  const rows = data?.processes ?? []
  const [peek, setPeek] = useState<MemProcess | null>(null)

  const pop = open ? (
        <div
          ref={popRef}
          className="mem-pop"
          role="dialog"
          aria-label={t('{title} 的記憶體明細', { title })}
          style={pos ? { left: pos.left, top: pos.top } : { visibility: 'hidden', left: 0, top: 0 }}
        >
          {hosts && hosts.length > 1 ? (
            <div className="mem-pop-hosts" role="tablist" aria-label={t('主機')}>
              {hosts.map((h) => (
                <button
                  key={h}
                  type="button"
                  role="tab"
                  aria-selected={h === host}
                  className={`mem-pop-host${h === host ? ' on' : ''}`}
                  onClick={() => {
                    if (h === host) return
                    setData(null)
                    setErr(null)
                    setHost(h)
                    void load(h)
                  }}
                >
                  {h === LOCAL_HOST ? t('本機') : `@${h}`}
                </button>
              ))}
            </div>
          ) : null}
          <div className="mem-pop-head">
            <span className="mem-pop-title">
              {title} RAM {humanBytes(row?.total_bytes ?? 0)} · herdr {humanBytes(row?.herdr_bytes ?? 0)} · {t('{n} 個 process', { n: row?.processes ?? 0 })}
            </span>
            <button type="button" className="mem-pop-refresh" disabled={loading} onClick={() => void load()}>
              {t('重新整理')}
            </button>
          </div>
          {row?.machine ? (
            <p className={`mem-pop-machine${row.machine.available_bytes / row.machine.total_bytes < 0.15 ? ' low' : ''}`}>
              {t.x('這台機器 剩 {free} / 共 {total}', {
                free: <strong>{humanBytes(row.machine.available_bytes)}</strong>,
                total: humanBytes(row.machine.total_bytes),
              })}
              <span className="mem-pop-machine-used">
                {t('（已用 {used}，其中 herdr 樹 {tree}）', {
                  used: humanBytes(row.machine.total_bytes - row.machine.available_bytes),
                  tree: humanBytes(row.total_bytes),
                })}
              </span>
              {typeof row.machine.cpu_pct === 'number' && Number.isFinite(row.machine.cpu_pct) ? (
                <span className={`mem-pop-cpu${Math.round(row.machine.cpu_pct) >= 85 ? ' hot' : ''}`}>
                  {' · '}
                  {t('整機 CPU：{pct}%', { pct: Math.round(row.machine.cpu_pct) })}
                </span>
              ) : null}
            </p>
          ) : null}
          {row?.browsers.length ? (
            <p className={`mem-pop-browsers${tabsTotal(row.browsers) >= TABS_WARN ? ' hot' : ''}`}>
              {t('瀏覽器 {info}', { info: browsersLine(row.browsers) })}
              {tabsTotal(row.browsers) >= TABS_WARN
                ? t('——超過 {warn} 個分頁，RAM 多半是它們吃的，關一些。', { warn: TABS_WARN })
                : t('（不算在上面的 RAM 裡）')}
            </p>
          ) : null}
          {err ? <p className="mem-pop-err" role="alert">{err}</p> : null}
          {!err && rows.length === 0 ? (
            <p className="mem-pop-empty">{loading ? t('取樣中…') : t('沒有程序。')}</p>
          ) : null}
          {rows.length > 0 ? (
            <table className="mem-pop-table">
              <thead>
                <tr>
                  <th>{t('大小')}</th>
                  <th>{t('程式')}</th>
                  <th>{t('誰的')}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {rows.map((p) => {
                  const canPeek = p.owner !== 'bot' && !!p.pane_id
                  return (
                    <tr key={p.pid} className={p.owner === 'bot' ? 'is-bot' : ''}>
                      <td className="mem-pop-size" title={t('自己 {self}，連同 {children} 個子程序', { self: humanBytes(p.rss_bytes), children: p.children })}>
                        {humanBytes(p.subtree_bytes)}
                      </td>
                      <td className="mem-pop-argv" title={p.argv}>
                        {p.argv}
                      </td>
                      <td className="mem-pop-owner">
                        {canPeek ? (
                          <button type="button" className="mem-pop-peek" title={t('開一個視窗看這個 pane 現在的畫面')} onClick={() => setPeek(p)}>
                            {ownerLabel(p, t)}
                            <span className="mem-pop-caret" aria-hidden="true">↗</span>
                          </button>
                        ) : (
                          ownerLabel(p, t)
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
          <div className="mem-pop-foot">
            <span className="hint">{t('結束的是那個 pane 裡的程序，pane 本身還在。')}</span>
          </div>
        </div>
      ) : null

  return (
    <div ref={wrap} className="mem-wrap">
      <button
        ref={btnRef}
        type="button"
        className="mem-open"
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={t('{title} 的記憶體明細', { title })}
        onClick={() => {
          if (!open) {
            setHost(initialHost)
            void load(initialHost)
          }
          setOpen(!open)
        }}
      >
        {children}
      </button>
      {pop ? createPortal(pop, document.body) : null}
      {peek ? <MemPaneModal host={host} p={peek} onClose={() => setPeek(null)} /> : null}
    </div>
  )
}
