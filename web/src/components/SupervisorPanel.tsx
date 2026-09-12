/**
 * AGM 總管的入口面板。
 *
 * 這裡**不做**新的聊天室：總管就是一顆 bot，要跟它說話請開它既有的對話。這個面板只
 * 負責「環境在什麼狀態、要不要建立／啟動、遠端入口通不通、手上有哪些交辦」。
 *
 * 端點還沒部署的 daemon 會讓 `fetchSupervisor()` 回 `null`——那不是錯誤，是「這台
 * daemon 還沒有這個功能」，畫面上要講清楚要做什麼才會有。
 */

import { useCallback, useEffect, useState } from 'react'
import { SUPERVISOR_CANDIDATES, fetchIncidents, fetchSupervisor, supervisorAction } from '../api/supervisor'
import type { SupervisorAction, SupervisorAssignment, SupervisorIncident, SupervisorInfo } from '../api/supervisor'
import { useStore } from '../store/store'
import './supervisor.css'

/** bot 起沒起來。跟遠端入口是兩件事，不要混在同一顆燈上。 */
const STATUS_LABEL: Record<string, string> = {
  not_configured: '尚未建立',
  stopped: '已停止',
  starting: '啟動中',
  running: '執行中',
  busy: '處理中',
  waiting_quota: '等額度恢復',
  failed: '啟動失敗',
  unknown: '狀態不明',
}

/** 樂觀色：只有真的在跑才給綠燈，`unknown` 一律當灰的。 */
const STATUS_TONE: Record<string, string> = {
  running: 'ok',
  busy: 'ok',
  starting: 'warn',
  waiting_quota: 'warn',
  failed: 'bad',
}

const REMOTE_LABEL: Record<string, string> = {
  active: '已連線',
  unverified: '未驗證',
  failed: '連線失敗',
  off: '未啟用',
  unknown: '狀態不明',
}

/**
 * 交辦的生命週期。`awaiting_review` 是這裡最重要的一格：回合跑完只到這裡，AGM 驗收過
 * 才會變 `completed`。把它畫成「已完成」就是在替沒人看過的工作背書。
 */
const ASSIGN_LABEL: Record<string, string> = {
  queued: '待送出',
  delivered: '已送達',
  unknown: '送達未知',
  awaiting_review: '等驗收',
  blocked: '阻塞中',
  completed: '已驗收',
  failed: '失敗',
  cancelled: '已取消',
  superseded: '已接續',
}

const ASSIGN_TONE: Record<string, string> = {
  completed: 'ok',
  failed: 'bad',
  awaiting_review: 'warn',
  blocked: 'warn',
  unknown: 'warn',
}

const INCIDENT_LABEL: Record<string, string> = {
  host_disconnected: '主機斷線',
  bot_stopped: 'bot 該開著卻停了',
  assignment_stalled: '交辦卡住沒進度',
  notify_exhausted: '通知送不出去',
  remote_entry: '遠端入口異常',
}

function label(map: Record<string, string>, key: string): string {
  return map[key] ?? key
}

function AssignmentRow({ item, botName }: { item: SupervisorAssignment; botName: string }) {
  return (
    <li className="agm-assign">
      <div className="agm-assign-head">
        <span className="agm-assign-to">{botName || item.target_bot_id || '（未知 bot）'}</span>
        <span className={`agm-chip ${ASSIGN_TONE[item.status] ?? ''}`}>{label(ASSIGN_LABEL, item.status)}</span>
        {/* 舊資料是在有驗收狀態之前就關掉的：關掉了，但沒有人驗收過，不要讓它看起來像有。 */}
        {item.legacy_closed ? <span className="agm-chip">舊資料·未經驗收</span> : null}
      </div>
      <p className="agm-assign-text">{item.text}</p>
      {/* turn_id 是這筆交辦唯一能拿去對帳的把手，短短一行也要留著。 */}
      {item.turn_id ? <p className="agm-assign-meta mono">turn {item.turn_id}</p> : null}
      {item.result ? <p className="agm-assign-meta">{item.result}</p> : null}
      {item.awaiting_review ? (
        <p className="agm-assign-meta">
          回合已結束（{item.turn_status || '狀態不明'}）
          {item.evidence_complete === false ? '，回覆是終端擷取的，可能不完整' : ''}
          ，等 AGM 驗收才算完成。
        </p>
      ) : null}
      {item.review.decision ? (
        <p className="agm-assign-meta">
          {item.review.by || 'AGM'} 判定 {item.review.decision}
          {item.review.reason ? `：${item.review.reason}` : ''}
        </p>
      ) : null}
    </li>
  )
}

function IncidentList({ items }: { items: SupervisorIncident[] }) {
  return (
    <section className="agm-sec">
      <h4>
        系統故障
        <span className={`agm-chip ${items.length > 0 ? 'bad' : 'ok'}`}>{items.length} 筆未恢復</span>
      </h4>
      {items.length === 0 ? (
        <p className="agm-note">目前沒有未恢復的故障。這一格看的是主機、bot 與交辦，不是 AGM 自己。</p>
      ) : (
        <ul className="agm-assign-list">
          {items.map((i) => (
            <li key={i.id} className="agm-assign">
              <div className="agm-assign-head">
                <span className="agm-assign-to">{label(INCIDENT_LABEL, i.kind)}</span>
                <span className={`agm-chip ${i.severity === 'critical' ? 'bad' : 'warn'}`}>{i.severity}</span>
              </div>
              <p className="agm-assign-meta mono">{i.resource}</p>
              <p className="agm-assign-meta">自 {i.first_seen_at} 起，已確認 {i.occurrences} 次</p>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}

export function SupervisorPanel({ onOpenChat }: { onOpenChat?: () => void }) {
  const [info, setInfo] = useState<SupervisorInfo | null | undefined>(undefined)
  /** `null` 有兩種意思，要分開存：還沒載完 vs. 這台 daemon 沒有這個 API。 */
  const [unsupported, setUnsupported] = useState(false)
  const [busy, setBusy] = useState<SupervisorAction | null>(null)
  const [error, setError] = useState('')
  const [incidents, setIncidents] = useState<SupervisorIncident[]>([])
  const selectBot = useStore((s) => s.selectBot)
  const bots = useStore((s) => s.bots)

  /**
   * 讀一次狀態。回傳而不是自己寫進 state，掛載時的那一次才能在面板已經關掉之後
   * 把結果丟掉（Modal 一關就 unmount，慢回來的請求不該再去戳已死的元件）。
   */
  const read = useCallback(async (): Promise<{ info: SupervisorInfo | null; unsupported: boolean; error: string }> => {
    try {
      const next = await fetchSupervisor()
      return { info: next, unsupported: next === null, error: '' }
    } catch (e) {
      return { info: null, unsupported: false, error: e instanceof Error ? e.message : String(e) }
    }
  }, [])

  const apply = useCallback((r: { info: SupervisorInfo | null; unsupported: boolean; error: string }) => {
    setUnsupported(r.unsupported)
    setInfo(r.info)
    setError(r.error)
  }, [])

  useEffect(() => {
    let alive = true
    void read().then((r) => {
      if (alive) apply(r)
    })
    // 故障清單獨立拉：它跟總管狀態是兩個問題，一邊掛了另一邊還要看得到。
    void fetchIncidents()
      .then((list) => {
        if (alive) setIncidents(list)
      })
      .catch(() => {})
    return () => {
      alive = false
    }
  }, [read, apply])

  const act = async (action: SupervisorAction) => {
    setBusy(action)
    setError('')
    try {
      setInfo(await supervisorAction(action))
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      // 失敗後重讀一次：daemon 那邊可能已經改了一半，畫面不能停在按下去前的樣子。
      const r = await read()
      if (r.info) setInfo(r.info)
    } finally {
      setBusy(null)
    }
  }

  if (unsupported) {
    return (
      <div className="agm-panel">
        <p className="agm-note">
          這台 daemon 還沒有總管功能。更新 <code>agents-managerd</code> 並重啟之後，這裡才會出現 AGM 的建立與啟動。
        </p>
      </div>
    )
  }

  if (info === undefined) return <p className="agm-note">載入中…</p>

  const live = info
  if (!live) {
    return (
      <div className="agm-panel">
        <p className="agm-error">讀不到總管狀態：{error || '未知錯誤'}</p>
        <button type="button" className="btn" onClick={() => void read().then(apply)}>
          重試
        </button>
      </div>
    )
  }

  const managerBot = bots.find((b) => b.id === live.bot_id)
  const awaitingReview = live.assignments.filter((a) => a.awaiting_review).length
  const activeIndex = SUPERVISOR_CANDIDATES.findIndex((c) => c.model === live.model)
  const running = live.status === 'running' || live.status === 'busy' || live.status === 'starting'

  return (
    <div className="agm-panel">
      <div className="agm-row">
        <span className="agm-name">AGM</span>
        <span className={`agm-chip ${STATUS_TONE[live.status] ?? ''}`}>{label(STATUS_LABEL, live.status)}</span>
        <span className="agm-chip mono">
          {live.identity} · {live.model} · {live.effort}
        </span>
      </div>

      {/* 候補順序是固定的，直接把兩個候選都畫出來，使用者才知道壞掉時會換去哪。 */}
      <ol className="agm-candidates">
        {SUPERVISOR_CANDIDATES.map((c, i) => (
          <li key={c.model} className={i === activeIndex ? 'on' : ''}>
            <span className="agm-cand-n">{i + 1}</span>
            <span className="mono">{c.label}</span>
            {i === activeIndex ? <span className="agm-cand-tag">使用中</span> : null}
          </li>
        ))}
      </ol>

      <div className="agm-row">
        <span className="agm-key">遠端入口</span>
        <span className={`agm-chip ${live.remote.status === 'active' ? 'ok' : live.remote.status === 'failed' ? 'bad' : 'warn'}`}>
          {label(REMOTE_LABEL, live.remote.status)}
        </span>
        {live.remote.url ? (
          <a className="agm-link" href={live.remote.url} target="_blank" rel="noreferrer">
            開啟連線
          </a>
        ) : (
          <span className="agm-note inline">尚未取得手機可用的入口</span>
        )}
      </div>
      {live.remote.status !== 'active' ? (
        <p className="agm-note">
          Remote Control 沒起來不代表 AGM 沒起來——bot 仍會照常收交辦，只是手機上暫時連不到這個對話。
        </p>
      ) : null}

      <div className="agm-actions">
        {!live.configured ? (
          <button type="button" className="btn primary" disabled={busy !== null} onClick={() => void act('setup')}>
            {busy === 'setup' ? '建立中…' : '建立 AGM 環境'}
          </button>
        ) : (
          <>
            <button type="button" className="btn" disabled={busy !== null || running} onClick={() => void act('start')}>
              {busy === 'start' ? '啟動中…' : '啟動'}
            </button>
            <button type="button" className="btn" disabled={busy !== null || !running} onClick={() => void act('stop')}>
              {busy === 'stop' ? '停止中…' : '停止'}
            </button>
            <button
              type="button"
              className="btn"
              disabled={busy !== null}
              title="改用第二順位 cc0 opus low。帳號額度耗盡時換模型也救不了。"
              onClick={() => void act('fallback')}
            >
              {busy === 'fallback' ? '切換中…' : '改用候補模型'}
            </button>
            {live.bot_id ? (
              <button
                type="button"
                className="btn"
                onClick={() => {
                  selectBot(live.bot_id)
                  onOpenChat?.()
                }}
              >
                打開 AGM 對話
              </button>
            ) : null}
          </>
        )}
      </div>

      {!live.configured ? (
        <p className="agm-note">
          建立只會準備專用工作目錄與 bot 設定，不會自己啟動；重複按也不會建出第二個。
        </p>
      ) : null}

      {error ? <p className="agm-error">{error}</p> : null}

      <IncidentList items={incidents} />

      <section className="agm-sec">
        <h4>
          交辦
          {live.pending_count > 0 ? <span className="agm-chip warn">{live.pending_count} 筆未結案</span> : null}
          {awaitingReview > 0 ? <span className="agm-chip warn">{awaitingReview} 筆等驗收</span> : null}
        </h4>
        {live.assignments.length === 0 ? (
          <p className="agm-note">目前沒有交辦。AGM 只有在你明確說「交給它」時才會派工。</p>
        ) : (
          <ul className="agm-assign-list">
            {live.assignments.map((a) => (
              <AssignmentRow key={a.id} item={a} botName={bots.find((b) => b.id === a.target_bot_id)?.name ?? ''} />
            ))}
          </ul>
        )}
      </section>

      {live.configured && !managerBot ? (
        <p className="agm-note">設定裡有 AGM，但目前的清單找不到這顆 bot（可能還沒同步或已被移除）。</p>
      ) : null}
    </div>
  )
}
