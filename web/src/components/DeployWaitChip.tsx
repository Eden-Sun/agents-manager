import { useEffect, useRef, useState } from 'react'
import { BLOCKER_WHY, dismissDeployWait, escalateDeployWait, type DeployWait } from '../api/deployWait'
import { setDeployWait, useDeployWait } from '../store/deployWait'
import { useStore } from '../store/store'
import './deployWait.css'

const hhmm = (iso: string | null) => (iso ? new Date(iso).toLocaleTimeString('zh-TW', { hour: '2-digit', minute: '2-digit', hour12: false }) : '')

/**
 * 側欄標題列的「⏳ 部署等 N 分」（SPEC §18.10，使用者 2026-10-04：等待部署超過 3 分鐘，馬上通知 user 調度）。
 * 只在 daemon 通知過、還在等、沒按「先等」時出現。點開：要上的 commit、等了多久、被誰擋（點名字跳過去）、預計幾點自動放寬，
 * 以及「現在換版」（這次部署直接放寬）與「先等」（收起來）。
 */
export function DeployWaitChip() {
  const wait = useDeployWait((s) => s.wait)
  const notify = useStore((s) => s.notify)
  const selectBot = useStore((s) => s.selectBot)
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const [now, setNow] = useState(() => Date.now())
  const wrap = useRef<HTMLSpanElement>(null)

  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 30_000)
    return () => clearInterval(t)
  }, [])
  useEffect(() => {
    if (!open) return
    const close = (e: MouseEvent) => !wrap.current?.contains(e.target as Node) && setOpen(false)
    const esc = (e: KeyboardEvent) => e.key === 'Escape' && setOpen(false)
    document.addEventListener('mousedown', close)
    document.addEventListener('keydown', esc)
    return () => {
      document.removeEventListener('mousedown', close)
      document.removeEventListener('keydown', esc)
    }
  }, [open])

  if (!wait || wait.phase !== 'waiting' || wait.dismissed) return null
  const since = Date.parse(wait.since)
  const mins = Math.max(0, Math.floor((Number.isFinite(since) ? now - since : wait.waited_secs * 1000) / 60_000))
  const run = async (what: () => Promise<DeployWait | null>, done: string) => {
    setBusy(true)
    try {
      const w = await what()
      if (w) setDeployWait(w)
      notify('info', done)
      setOpen(false)
    } catch (e) {
      notify('error', `沒能送出：${e instanceof Error ? e.message : String(e)}`)
    } finally {
      setBusy(false)
    }
  }

  return (
    <span className="deploy-wait" ref={wrap}>
      <button
        type="button"
        className="deploy-wait-chip"
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={`部署 ${wait.commit.slice(0, 8)} 等換版窗口 ${mins} 分鐘，點開調度`}
        title={wait.summary}
        onClick={() => setOpen((o) => !o)}
      >
        ⏳ 部署等 {mins} 分
      </button>
      {open ? (
        <div className="deploy-wait-pop" role="dialog" aria-label="部署等換版窗口">
          <p>
            要上 <code>{wait.commit.slice(0, 8)}</code>，已等 <strong>{mins}</strong> 分鐘。
          </p>
          {wait.blockers.length ? (
            <p>
              被擋住：
              {wait.blockers.map((b, i) => (
                <span key={`${b.why}-${b.bot_id ?? b.name}`}>
                  {i ? '、' : ''}
                  {b.bot_id ? (
                    <button
                      type="button"
                      className="deploy-wait-bot"
                      onClick={() => {
                        selectBot(b.bot_id)
                        setOpen(false)
                      }}
                    >
                      {b.name}
                    </button>
                  ) : (
                    <span>{b.name}</span>
                  )}
                  （{BLOCKER_WHY[b.why] ?? b.why}）
                </span>
              ))}
            </p>
          ) : null}
          <p className="deploy-wait-note">
            {wait.user_escalated
              ? '已按「現在換版」：working 不再擋，送達中、別人握著窗口、讀不到狀態照樣擋。'
              : wait.escalates_at
                ? `沒動作的話約 ${hhmm(wait.escalates_at)} 自動放寬（working 不再擋）。`
                : '已自動放寬，只剩送達中、別人握著窗口或讀不到狀態在擋。'}
          </p>
          <div className="deploy-wait-actions">
            <button type="button" className="btn" disabled={busy} onClick={() => void run(() => dismissDeployWait(wait.id), '先等：部署照樣在等，到時候自動放寬')}>
              先等
            </button>
            <button
              type="button"
              className="btn primary"
              disabled={busy || wait.user_escalated}
              onClick={() => void run(() => escalateDeployWait(wait.id), '已放寬這次部署：working 不再擋，下一次試窗口就換版')}
            >
              現在換版
            </button>
          </div>
        </div>
      ) : null}
    </span>
  )
}
