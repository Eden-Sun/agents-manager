/**
 * AGM 總管的入口面板。
 *
 * **目前沒有掛在任何地方**（2026-10-04 使用者：這個功能沒用、從來沒點過，側欄入口與 Modal 拿掉）。元件、
 * `api/supervisor.ts` 與 daemon 端點都留著；要的話把 `<SupervisorPanel />` 放進 Modal 就回來。
 * 同樣的操作 CLI 都有：`agm supervisor`（狀態）、`agm supervisor-{setup,start,stop,fallback}`、`agm responder …`、`agm health`。
 *
 * 這裡**不做**新的聊天室：總管就是一顆 bot，要跟它說話請開它既有的對話。這個面板只
 * 負責「環境在什麼狀態、要不要建立／啟動、手上有哪些交辦」。
 *
 * 端點還沒部署的 daemon 會讓 `fetchSupervisor()` 回 `null`——那不是錯誤，是「這台
 * daemon 還沒有這個功能」，畫面上要講清楚要做什麼才會有。
 */

import { useCallback, useEffect, useState } from 'react'
import { SUPERVISOR_CANDIDATES, fetchIncidents, fetchSupervisor, supervisorAction } from '../api/supervisor'
import type { SupervisorAction, SupervisorAssignment, SupervisorIncident, SupervisorInfo } from '../api/supervisor'
import { msg, tr, useT } from '../lib/i18n'
import { ASSIGN_LABEL, INCIDENT_LABEL } from '../lib/supervisorLabels'
import { useStore } from '../store/store'
import './supervisor.css'

/** bot 起沒起來。 */
const STATUS_LABEL: Record<string, string> = {
  not_configured: msg('尚未建立'),
  stopped: msg('已停止'),
  starting: msg('啟動中'),
  running: msg('執行中'),
  busy: msg('處理中'),
  idle: msg('待命中'),
  waiting_quota: msg('等額度恢復'),
  missing: msg('bot 不見了'),
  failed: msg('啟動失敗'),
  unknown: msg('狀態不明'),
}

/** 樂觀色：只有真的在跑才給綠燈，`unknown` 一律當灰的。 */
const STATUS_TONE: Record<string, string> = {
  running: 'ok',
  busy: 'ok',
  idle: 'ok',
  starting: 'warn',
  waiting_quota: 'warn',
  failed: 'bad',
  missing: 'bad',
}

const ASSIGN_TONE: Record<string, string> = {
  completed: 'ok',
  failed: 'bad',
  awaiting_review: 'warn',
  blocked: 'warn',
  quota_blocked: 'warn',
  unknown: 'warn',
}


/** 故障清單有三種狀態，而「還沒讀到」跟「讀到了，沒有故障」絕對不能畫成同一種。 */
type IncidentState =
  | { kind: 'loading' }
  | { kind: 'ok'; items: SupervisorIncident[] }
  | { kind: 'error'; message: string }
  /** 這台 daemon 還沒有這個端點：不是沒故障，是量不到。 */
  | { kind: 'unsupported' }

/** 標籤表的值是 `msg()` 標記過的 key，顯示時查翻譯；沒有對到就原樣顯示。 */
function label(map: Record<string, string>, key: string): string {
  return map[key] === undefined ? key : tr(map[key])
}

/**
 * 兩個角色各做什麼、各被叫醒幾次。巡檢就是上面那顆 AGM（使用者入口）；協調者只回應
 * bot。協調者停了或沒額度時事件留在 inbox，不會改由巡檢處理——這一行要讓人看得出「在等」而不是「壞了」。
 */
function RoleSummary({ live, onOpen }: { live: SupervisorInfo; onOpen: (botId: string) => void }) {
  const t = useT()
  const r = live.responder
  const wake = (st: SupervisorInfo['stats']) =>
    st.wakes > 0
      ? st.last_wake_reason
        ? t('叫醒 {n} 次・最近：{reason}', { n: st.wakes, reason: st.last_wake_reason })
        : t('叫醒 {n} 次', { n: st.wakes })
      : t('還沒被叫醒過')
  return (
    <section className="agm-sec">
      <h4>{t('角色')}</h4>
      <div className="agm-row">
        <span className="agm-key">{t('巡檢')}</span>
        <span className="agm-chip mono">{live.identity} · {live.model} · {live.effort}</span>
        <span className="agm-note inline">{t('使用者入口・健康與故障')}</span>
      </div>
      <p className="agm-note">
        {wake(live.stats)}
        {live.stats.merged > 0 ? t('・合併重複 {n} 則', { n: live.stats.merged }) : ''}
      </p>
      <div className="agm-row">
        <span className="agm-key">{t('協調')}</span>
        {r.configured ? (
          <>
            <span className={`agm-chip ${STATUS_TONE[r.status] ?? ''}`}>{label(STATUS_LABEL, r.status)}</span>
            {/* 設定值；實際在跑的不一樣時兩個都畫出來，不要只給一個會騙人的數字。 */}
            <span className="agm-chip mono">{r.identity} · {r.model} · {r.effort}</span>
            {r.runtime.model && (r.runtime.model !== r.model || (r.runtime.effort && r.runtime.effort !== r.effort)) ? (
              <span className="agm-chip warn mono">
                {r.runtime.effort
                  ? t('實際 {model} · {effort}', { model: r.runtime.model, effort: r.runtime.effort })
                  : t('實際 {model}', { model: r.runtime.model })}
              </span>
            ) : null}
            {r.inbox_open > 0 ? <span className="agm-chip warn">{t('{n} 則待處理', { n: r.inbox_open })}</span> : null}
            {r.bot_id && r.bot_present ? (
              <button type="button" className="btn" onClick={() => onOpen(r.bot_id)}>
                {t('打開協調者對話')}
              </button>
            ) : null}
          </>
        ) : (
          <span className="agm-note inline">{t('尚未建立：bot 的申請仍由巡檢處理（`agm responder setup` 後再 start）')}</span>
        )}
      </div>
      {r.configured ? (
        <p className="agm-note">
          {t('只回應 bot（申請、交辦回報、核准、任務）。')}
          {wake(r.stats)}
          {r.stats.duplicates > 0 ? t('・擋下重複申請 {n} 則', { n: r.stats.duplicates }) : ''}
        </p>
      ) : null}
      {r.status === 'waiting_quota' ? (
        <p className="agm-note">
          {t('{detail}。事件留在 inbox 等它恢復，不會轉給巡檢。', { detail: r.status_detail || t('協調者額度見底') })}
          {r.stats.notify_next_at ? t('下次重試 {time}', { time: r.stats.notify_next_at }) : ''}
        </p>
      ) : r.status_detail && r.configured ? (
        <p className="agm-note">{r.status_detail}</p>
      ) : null}
    </section>
  )
}

function AssignmentRow({ item, botName }: { item: SupervisorAssignment; botName: string }) {
  const t = useT()
  return (
    <li className="agm-assign">
      <div className="agm-assign-head">
        <span className="agm-assign-to">{botName || item.target_bot_id || t('（未知 bot）')}</span>
        <span className={`agm-chip ${ASSIGN_TONE[item.status] ?? ''}`}>{label(ASSIGN_LABEL, item.status)}</span>
        {/* 舊資料是在有驗收狀態之前就關掉的：關掉了，但沒有人驗收過，不要讓它看起來像有。 */}
        {item.legacy_closed ? <span className="agm-chip">{t('舊資料·未經驗收')}</span> : null}
      </div>
      <p className="agm-assign-text">{item.text}</p>
      {/* turn_id 是這筆交辦唯一能拿去對帳的把手，短短一行也要留著。 */}
      {item.turn_id ? <p className="agm-assign-meta mono">turn {item.turn_id}</p> : null}
      {item.result ? <p className="agm-assign-meta">{item.result}</p> : null}
      {item.awaiting_review ? (
        <p className="agm-assign-meta">
          {t('回合已結束（{status}）', { status: item.turn_status || t('狀態不明') })}
          {item.evidence_complete === false ? t('，回覆是終端擷取的，可能不完整') : ''}
          {t('，等 AGM 驗收才算完成。')}
        </p>
      ) : null}
      {item.review.decision ? (
        <p className="agm-assign-meta">
          {t('{by} 判定 {decision}', { by: item.review.by || 'AGM', decision: item.review.decision })}
          {item.review.reason ? t('：{reason}', { reason: item.review.reason }) : ''}
        </p>
      ) : null}
    </li>
  )
}

/**
 * 讀不到就說讀不到。
 *
 * 原本 incidents 初始是 `[]`、`fetchIncidents` 的 catch 又把錯誤吃掉，於是 API 掛掉時這一格
 * 顯示「0 筆未恢復」配綠燈——在系統最可能真的出事的時候，畫面最像沒事。這正是這個面板要修的
 * 那個毛病本身。
 */
function IncidentList({ state }: { state: IncidentState }) {
  const t = useT()
  if (state.kind !== 'ok') {
    const [tone, text] =
      state.kind === 'loading'
        ? ['', t('讀取中…')]
        : state.kind === 'unsupported'
          ? ['warn', t('這台 daemon 還沒有故障清單端點，所以這裡沒有數字——不是沒有故障。')]
          : ['bad', t('讀不到故障清單：{message}。這不代表系統沒事，代表現在量不到。', { message: state.message })]
    return (
      <section className="agm-sec">
        <h4>
          {t('系統故障')}
          <span className={`agm-chip ${tone}`}>{t('狀態不明')}</span>
        </h4>
        <p className="agm-note">{text}</p>
      </section>
    )
  }
  const items = state.items
  return (
    <section className="agm-sec">
      <h4>
        {t('系統故障')}
        <span className={`agm-chip ${items.length > 0 ? 'bad' : 'ok'}`}>{t('{n} 筆未恢復', { n: items.length })}</span>
      </h4>
      {items.length === 0 ? (
        <p className="agm-note">{t('目前沒有未恢復的故障。這一格看的是主機、bot 與交辦，不是 AGM 自己。')}</p>
      ) : (
        <ul className="agm-assign-list">
          {items.map((i) => (
            <li key={i.id} className="agm-assign">
              <div className="agm-assign-head">
                <span className="agm-assign-to">{label(INCIDENT_LABEL, i.kind)}</span>
                <span className={`agm-chip ${i.severity === 'critical' ? 'bad' : 'warn'}`}>{i.severity}</span>
              </div>
              <p className="agm-assign-meta mono">{i.resource}</p>
              <p className="agm-assign-meta">{t('自 {time} 起，已確認 {n} 次', { time: i.first_seen_at, n: i.occurrences })}</p>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}

export function SupervisorPanel({ onOpenChat }: { onOpenChat?: () => void }) {
  const t = useT()
  const [info, setInfo] = useState<SupervisorInfo | null | undefined>(undefined)
  /** `null` 有兩種意思，要分開存：還沒載完 vs. 這台 daemon 沒有這個 API。 */
  const [unsupported, setUnsupported] = useState(false)
  const [busy, setBusy] = useState<SupervisorAction | null>(null)
  const [error, setError] = useState('')
  const [incidents, setIncidents] = useState<IncidentState>({ kind: 'loading' })
  const selectBot = useStore((s) => s.selectBot)
  const bots = useStore((s) => s.bots)
  const rev = useStore((s) => s.supervisorRev)

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
    // 失敗不能靜靜吞掉——吞掉就會變成「0 筆未恢復」的綠燈。
    void fetchIncidents()
      .then((r) => {
        if (alive) setIncidents(r === null ? { kind: 'unsupported' } : { kind: 'ok', items: r })
      })
      .catch((e: unknown) => {
        if (alive) setIncidents({ kind: 'error', message: e instanceof Error ? e.message : String(e) })
      })
    return () => {
      alive = false
    }
  }, [read, apply])

  // 面板開著時 daemon 推 `supervisor_changed`（交辦、驗收、額度、看門狗…）：重讀狀態與故障清單。以前只在掛載時讀一次，
  // 開著面板等 AGM 起來或交辦跑完，畫面一直停在按開的那一刻。節流成一秒一次（一陣事件只讀一輪）；第一次由上面的掛載 effect 讀。
  useEffect(() => {
    if (rev === 0) return
    let alive = true
    const timer = setTimeout(() => {
      void read().then((r) => {
        // 讀失敗不蓋掉已經在畫面上的狀態（暫時性的錯誤不該把面板換成「讀不到」）。
        if (alive && r.info) apply(r)
      })
      void fetchIncidents().then(
        (r) => {
          if (alive && r !== null) setIncidents({ kind: 'ok', items: r })
        },
        () => {},
      )
    }, 1000)
    return () => {
      alive = false
      clearTimeout(timer)
    }
  }, [rev, read, apply])

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
          {t.x('這台 daemon 還沒有總管功能。更新 {bin} 並重啟之後，這裡才會出現 AGM 的建立與啟動。', {
            bin: <code>agents-managerd</code>,
          })}
        </p>
      </div>
    )
  }

  if (info === undefined) return <p className="agm-note">{t('載入中…')}</p>

  const live = info
  if (!live) {
    return (
      <div className="agm-panel">
        <p className="agm-error">{t('讀不到總管狀態：{error}', { error: error || t('未知錯誤') })}</p>
        <button type="button" className="btn" onClick={() => void read().then(apply)}>
          {t('重試')}
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
            {i === activeIndex ? <span className="agm-cand-tag">{t('使用中')}</span> : null}
          </li>
        ))}
      </ol>

      <RoleSummary live={live} onOpen={(id) => {
        selectBot(id)
        onOpenChat?.()
      }} />

      <div className="agm-actions">
        {!live.configured ? (
          <button type="button" className="btn primary" disabled={busy !== null} onClick={() => void act('setup')}>
            {busy === 'setup' ? t('建立中…') : t('建立 AGM 環境')}
          </button>
        ) : (
          <>
            <button type="button" className="btn" disabled={busy !== null || running} onClick={() => void act('start')}>
              {busy === 'start' ? t('啟動中…') : t('啟動')}
            </button>
            <button type="button" className="btn" disabled={busy !== null || !running} onClick={() => void act('stop')}>
              {busy === 'stop' ? t('停止中…') : t('停止')}
            </button>
            <button
              type="button"
              className="btn"
              disabled={busy !== null}
              title={t('改用第二順位 cc0 opus low。帳號額度耗盡時換模型也救不了。')}
              onClick={() => void act('fallback')}
            >
              {busy === 'fallback' ? t('切換中…') : t('改用候補模型')}
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
                {t('打開 AGM 對話')}
              </button>
            ) : null}
          </>
        )}
      </div>

      {!live.configured ? (
        <p className="agm-note">
          {t('建立只會準備專用工作目錄與 bot 設定，不會自己啟動；重複按也不會建出第二個。')}
        </p>
      ) : null}

      {error ? <p className="agm-error">{error}</p> : null}

      <IncidentList state={incidents} />

      <section className="agm-sec">
        <h4>
          {t('交辦')}
          {live.pending_count > 0 ? <span className="agm-chip warn">{t('{n} 筆未結案', { n: live.pending_count })}</span> : null}
          {awaitingReview > 0 ? <span className="agm-chip warn">{t('{n} 筆等驗收', { n: awaitingReview })}</span> : null}
        </h4>
        {live.assignments.length === 0 ? (
          <p className="agm-note">{t('目前沒有交辦。AGM 只有在你明確說「交給它」時才會派工。')}</p>
        ) : (
          <ul className="agm-assign-list">
            {live.assignments.map((a) => (
              <AssignmentRow key={a.id} item={a} botName={bots.find((b) => b.id === a.target_bot_id)?.name ?? ''} />
            ))}
          </ul>
        )}
      </section>

      {live.configured && !managerBot ? (
        <p className="agm-note">{t('設定裡有 AGM，但目前的清單找不到這顆 bot（可能還沒同步或已被移除）。')}</p>
      ) : null}
    </div>
  )
}
