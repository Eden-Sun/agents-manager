/**
 * 群組裡的任務條（`docs/goals/agm-missions.md` §5）：進行中的任務卡，加上一份可以展開的
 * 「已完成任務」清單。
 *
 * 位置刻意放在**輸入框正上方**而不是訊息列上方：任務卡是要動手的東西——停下來問人的時候
 * 要在這裡回答——跟 IssuesBar 那種「這個專案有什麼可以挑」不一樣，捲到哪裡都不該找不到它。
 */
import { useEffect, useMemo, useRef, useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Mission, MissionDetail } from '../api/types'
import {
  missionQna,
  assignmentLabel,
  assignmentOpen,
  deliveryLabel,
  missionView,
  pausedLabel,
  phaseLabel,
  quotaWaitText,
  MISSION_PHASES,
  PHASE_LABEL,
  ROLE_LABEL,
} from '../lib/missionView'
import { useStore } from '../store/store'
import { KindIcon } from './KindTag'
import './missions.css'

/** 一次最多攤開幾張進行中的卡；其餘收在「還有 N 個」後面。 */
const OPEN_SHOWN = 2

export function MissionsBar({ projectId }: { projectId: string }) {
  const supported = useStore((s) => s.missionsSupported)
  const missions = useStore(useShallow((s) => s.missions[projectId] ?? []))
  const capped = useStore((s) => s.missionsCapped[projectId])
  const loadMissions = useStore((s) => s.loadMissions)

  useEffect(() => {
    if (supported) void loadMissions(projectId)
  }, [projectId, supported, loadMissions])

  const [showAll, setShowAll] = useState(false)
  const [showDone, setShowDone] = useState(false)
  const [showCancelled, setShowCancelled] = useState(false)
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set())
  const [target, setTarget] = useState<string | null>(null)
  const [navigationError, setNavigationError] = useState('')
  const section = useRef<HTMLElement>(null)
  const setExpandedRow = (id: string, open: boolean) => setExpanded((old) => {
    const next = new Set(old)
    if (open) next.add(id)
    else next.delete(id)
    return next
  })
  const navigate = async (id: string) => {
    setNavigationError('')
    // A referenced result can fall outside the latest 100 missions. Fetch by ID first.
    await useStore.getState().loadMission(id)
    const state = useStore.getState()
    const detail = state.missionDetail[id]
    if (!detail) {
      setNavigationError(state.missionLoadErrors[id] || '無法讀取這筆任務，請再點一次重試')
      return
    }
    if (detail.status === 'done') setShowDone(true)
    else if (detail.status === 'cancelled') setShowCancelled(true)
    else setShowAll(true)
    setExpandedRow(id, true)
    setTarget(id)
  }
  useEffect(() => {
    if (!target) return
    const el = section.current?.querySelector<HTMLElement>(`[data-mission-id="${CSS.escape(target)}"]`)
    if (el) {
      el.scrollIntoView({ block: 'nearest' })
      el.focus({ preventScroll: true })
      setTarget(null)
    }
  }, [target, missions])

  /**
   * 「已完成任務」只算 `done`（P4 驗收缺陷 5）：API.md 寫得很清楚「已完成任務清單＝
   * `status=done`」，取消掉的擺進去會讓那份清單不能當成「做完了什麼」來讀。
   * 取消的另外列一段——做過什麼也是紀錄，只是不算成果。
   */
  const { open, done, cancelled } = useMemo(() => {
    const open: Mission[] = []
    const done: Mission[] = []
    const cancelled: Mission[] = []
    for (const m of missions) {
      if (m.status === 'done') done.push(m)
      else if (m.status === 'cancelled') cancelled.push(m)
      else open.push(m)
    }
    return { open, done, cancelled }
  }, [missions])

  if (!supported || (open.length === 0 && done.length === 0 && cancelled.length === 0)) return null
  // 停下來問人的先排前面——那是唯一在等使用者的東西。
  const sorted = [...open].sort((a, b) => Number(b.status === 'paused') - Number(a.status === 'paused'))
  const shown = showAll ? sorted : sorted.slice(0, OPEN_SHOWN)
  const hidden = sorted.length - shown.length

  return (
    <section ref={section} className="missions-bar" aria-label="群組任務">
      {navigationError ? <p className="mission-hint" role="alert">{navigationError}</p> : null}
      {shown.map((m) => (
        <MissionCard key={m.id} mission={m} onNavigate={navigate} />
      ))}
      {hidden > 0 ? (
        <button type="button" className="mini-btn mission-more" onClick={() => setShowAll(true)}>
          還有 {hidden} 個進行中的任務
        </button>
      ) : null}
      {showAll && sorted.length > OPEN_SHOWN ? (
        <button type="button" className="mini-btn mission-more" onClick={() => setShowAll(false)}>
          收起
        </button>
      ) : null}

      {done.length > 0 ? (
        <div className="mission-done">
          <button
            type="button"
            className="mission-done-head"
            aria-expanded={showDone}
            onClick={() => setShowDone((v) => !v)}
          >
            <span aria-hidden="true">{showDone ? '▾' : '▸'}</span>
            {capped?.done ? `已完成任務（最近 ${done.length} 筆）` : `已完成任務（${done.length}）`}
          </button>
          {showDone ? (
            <ul className="mission-done-list">
              {done.map((m) => (
                <MissionDoneRow key={m.id} mission={m} open={expanded.has(m.id)} onToggle={(open) => setExpandedRow(m.id, open)} onNavigate={navigate} />
              ))}
            </ul>
          ) : null}
        </div>
      ) : null}

      {cancelled.length > 0 ? (
        <div className="mission-done">
          <button
            type="button"
            className="mission-done-head"
            aria-expanded={showCancelled}
            title="取消掉的任務不算「已完成」，但做過什麼也是紀錄"
            onClick={() => setShowCancelled((v) => !v)}
          >
            <span aria-hidden="true">{showCancelled ? '▾' : '▸'}</span>
            {capped?.cancelled ? `已取消（最近 ${cancelled.length} 筆）` : `已取消（${cancelled.length}）`}
          </button>
          {showCancelled ? (
            <ul className="mission-done-list">
              {cancelled.map((m) => (
                <MissionDoneRow key={m.id} mission={m} open={expanded.has(m.id)} onToggle={(open) => setExpandedRow(m.id, open)} onNavigate={navigate} />
              ))}
            </ul>
          ) : null}
        </div>
      ) : null}
    </section>
  )
}

/**
 * 已完成的一列：原始指示、結果摘要、交付去哪裡（D1「一份可回顧的完成清單」）。
 *
 * 點開才去抓事件串——清單可能有幾十筆，一次全抓只是把 daemon 打爆。
 */
type NavigateMission = (id: string) => void

function MissionDoneRow({ mission, open, onToggle, onNavigate }: {
  mission: Mission
  open: boolean
  onToggle: (open: boolean) => void
  onNavigate: NavigateMission
}) {
  const detail = useStore((s) => s.missionDetail[mission.id] ?? null)
  const loadMission = useStore((s) => s.loadMission)
  const view = missionView(mission, detail?.events ?? [], detail?.assignments ?? [])

  return (
    <li tabIndex={-1} data-mission-id={mission.id} className={`mission-done-row${mission.status === 'cancelled' ? ' cancelled' : ''}`}>
      <button
        type="button"
        className="mission-done-row-head"
        aria-expanded={open}
        onClick={() => {
          onToggle(!open)
          if (!open) void loadMission(mission.id)
        }}
      >
        <span className="mission-done-mark" aria-hidden="true">
          {mission.status === 'cancelled' ? '⊘' : '✔'}
        </span>
        <span className="mission-done-text">{mission.text}</span>
        <span className="mission-done-when">{shortTime(mission.completed_at ?? mission.cancelled_at)}</span>
      </button>
      {open ? (
        <div className="mission-done-body">
          {mission.result_summary ? <p className="mission-summary">{mission.result_summary}</p> : null}
          <Evidence view={view} mission={mission} />
          <MissionLoadState missionId={mission.id} />
          <ParentLink mission={mission} detail={detail} onNavigate={onNavigate} />
          {detail ? <FollowUp mission={mission} detail={detail} onNavigate={onNavigate} /> : null}
        </div>
      ) : null}
    </li>
  )
}

/**
 * 成果卡下面的追問與追加修改。
 *
 * 兩件事刻意分開，因為後果不同：追問只是問一句話（不會改碼、不會產生新交付），追加修改會**開一筆
 * 新任務**。把它們做成同一個輸入框，使用者就沒辦法在按下去之前知道自己要的是哪一種。
 */
function FollowUp({ mission, detail, onNavigate }: { mission: Mission; detail: MissionDetail; onNavigate: NavigateMission }) {
  const askMission = useStore((s) => s.askMission)
  const reviseMission = useStore((s) => s.reviseMission)
  const [mode, setMode] = useState<'ask' | 'revise' | null>(null)
  const [text, setText] = useState('')
  const [busy, setBusy] = useState(false)
  const qna = missionQna(detail.events)
  // 已經有一輪續作還沒結束時，不要再給第二個入口——daemon 也會擋（revision_in_progress）。
  const openRevision = detail.revisions.find((r) => r.status !== 'done' && r.status !== 'cancelled') ?? null

  const send = async () => {
    const body = text.trim()
    if (!body || busy || !mode) return
    setBusy(true)
    const ok = mode === 'revise' ? Boolean(await reviseMission(mission.id, body)) : await askMission(mission.id, body)
    setBusy(false)
    // 失敗就把字留著——重打一次很煩，而且失敗多半是暫時的。
    if (ok) {
      setText('')
      setMode(null)
    }
  }

  return (
    <div className="mission-followup">
      {qna.length > 0 ? (
        <ul className="mission-qna">
          {qna.map((pair) => (
            <li key={pair.question.id}>
              <p className="mission-qna-q">{pair.question.text}</p>
              {pair.answer ? (
                <p className="mission-qna-a">{pair.answer.text}</p>
              ) : (
                <p className="mission-hint">等 AGM 回覆…</p>
              )}
            </li>
          ))}
        </ul>
      ) : null}

      {/* 點連結會展開並捲到同一份清單裡的任務，不另建一套頁面。 */}
      {detail.revisions.length > 0 ? (
        <ul className="mission-revisions">
          {detail.revisions.map((r) => (
            <li key={r.id}>
              <span className="mission-tag">
                {r.status === 'done' ? '續作已完成' : r.status === 'cancelled' ? '續作已取消' : '續作進行中'}
              </span>
              <button type="button" className="mission-link" onClick={() => onNavigate(r.id)}>
                查看續作：{r.text}
              </button>
            </li>
          ))}
        </ul>
      ) : null}

      {mode ? (
        <div className="mission-followup-form">
          <textarea
            className="mission-followup-input"
            value={text}
            aria-label={mode === 'ask' ? '追問內容' : '追加修改內容'}
            disabled={busy}
            rows={2}
            placeholder={mode === 'ask' ? '想問什麼？（不會改動任何東西）' : '要再改什麼？會開一筆新任務'}
            onChange={(e) => setText(e.target.value)}
          />
          <div className="mission-followup-actions">
            <button type="button" className="btn" disabled={busy} onClick={() => setMode(null)}>
              取消
            </button>
            <button type="button" className="btn primary" disabled={busy || !text.trim()} onClick={() => void send()}>
              {busy ? '送出中…' : mode === 'ask' ? '送出追問' : '建立續作'}
            </button>
          </div>
        </div>
      ) : (
        <div className="mission-followup-actions">
          <button type="button" className="btn" onClick={() => setMode('ask')}>
            追問
          </button>
          {mission.status === 'done' ? openRevision ? (
            <button type="button" className="mission-link" onClick={() => onNavigate(openRevision.id)}>
              查看進行中的續作
            </button>
          ) : (
            <button type="button" className="btn" onClick={() => setMode('revise')}>
              追加修改
            </button>
          ) : null}
        </div>
      )}
    </div>
  )
}

function MissionLoadState({ missionId }: { missionId: string }) {
  const error = useStore((s) => s.missionLoadErrors[missionId])
  const loading = useStore((s) => s.missionLoading[missionId])
  const loadMission = useStore((s) => s.loadMission)
  if (error) return <div className="mission-hint" role="alert">
    載入任務失敗：{error}
    <button type="button" className="mission-link" onClick={() => void loadMission(missionId)}>重試</button>
  </div>
  return loading ? <p className="mission-hint" role="status">讀取中…</p> : null
}

function ParentLink({ mission, detail, onNavigate }: {
  mission: Mission
  detail: MissionDetail | null
  onNavigate: NavigateMission
}) {
  if (!mission.parent_mission_id) return null
  if (detail?.parent?.missing) return <p className="mission-hint">原成果已不存在</p>
  return <button type="button" className="mission-link" onClick={() => onNavigate(mission.parent_mission_id!)}>
    查看原成果{detail?.parent?.text ? `：${detail.parent.text}` : ''}
  </button>
}

/** 驗證證據與交付去向（commit / PR），兩邊的卡片共用。 */
function Evidence({ view, mission }: { view: ReturnType<typeof missionView>; mission: Mission }) {
  if (!view.verified && !view.delivered) return null
  return (
    <ul className="mission-evidence">
      {view.verified ? (
        <li>
          <span className="mission-tag ok">驗證</span>
          {view.verified.text || '已驗證'}
        </li>
      ) : null}
      {view.delivered ? (
        <li>
          <span className="mission-tag">{deliveryLabel(view.delivered.mode)}</span>
          {view.delivered.url ? (
            <a href={view.delivered.url} target="_blank" rel="noreferrer">
              {view.delivered.branch ?? view.delivered.url}
            </a>
          ) : (
            <code>{view.delivered.sha?.slice(0, 12) ?? view.delivered.branch ?? '已交付'}</code>
          )}
        </li>
      ) : null}
      {!view.verified && mission.status === 'done' ? (
        <li className="mission-hint">（這一筆沒有驗證紀錄）</li>
      ) : null}
    </ul>
  )
}

/** 進行中的一張卡。 */
function MissionCard({ mission, onNavigate }: { mission: Mission; onNavigate: NavigateMission }) {
  const detail = useStore((s) => s.missionDetail[mission.id] ?? null)
  const loadMission = useStore((s) => s.loadMission)
  const loadError = useStore((s) => s.missionLoadErrors[mission.id])
  const controlMission = useStore((s) => s.controlMission)
  const answerMission = useStore((s) => s.answerMission)
  const [answer, setAnswer] = useState('')
  const [sending, setSending] = useState(false)

  // 進行中的卡一定要有事件串才畫得出進度與角色，進來就抓一次；之後靠 WS `mission_updated`。
  useEffect(() => {
    if (!detail && !loadError) void loadMission(mission.id)
  }, [mission.id, detail, loadError, loadMission])

  const view = missionView(mission, detail?.events ?? [], detail?.assignments ?? [])
  // 「在等人」的三種都用同一種醒目底色：等你回答、等額度、等 AGM——共同點是它自己不會動。
  const paused = view.phase === 'paused'
  const waiting = paused || view.phase === 'waiting_quota' || view.phase === 'awaiting_agm'

  return (
    <article tabIndex={-1} data-mission-id={mission.id} className={`mission-card${paused ? ' paused' : waiting ? ' waiting' : ''}`}>
      <header className="mission-head">
        <span className={`mission-phase ${view.phase}`}>{phaseLabel(view.phase)}</span>
        <span className="mission-text">{mission.text}</span>
        <span className="mission-opts">
          <span className="mission-tag" title="交付方式">
            {deliveryLabel(mission.delivery_mode)}
          </span>
          <span className="mission-tag" title="執行者 kind">
            <KindIcon kind={mission.executor_kind} />
            {mission.executor_kind}
          </span>
          {view.rounds.max > 0 ? (
            <span className="mission-tag" title="reviewer 退回／驗證失敗用掉的來回次數">
              來回 {view.rounds.used}/{view.rounds.max}
            </span>
          ) : null}
        </span>
      </header>

      <ParentLink mission={mission} detail={detail} onNavigate={onNavigate} />
      <MissionLoadState missionId={mission.id} />
      <ol className="mission-steps" aria-label="進度">
        {MISSION_PHASES.map((p, i) => (
          <li
            key={p}
            className={`mission-step${i < view.step ? ' past' : i === view.step ? ' at' : ''}${waiting && i === view.step ? ' stopped' : ''}`}
          >
            <span className="mission-dot" aria-hidden="true" />
            {PHASE_LABEL[p]}
          </li>
        ))}
      </ol>

      {view.actors.length > 0 || view.soloReview ? (
        <ul className="mission-actors">
          {view.actors.map((a) => (
            <li key={a.role}>
              <span className="mission-role">{ROLE_LABEL[a.role]}</span>
              <span className="mission-who">
                {a.bot ?? '—'}
                {a.identity ? ` · ${a.identity}` : ''}
                {a.model ? ` · ${a.model}` : ''}
              </span>
            </li>
          ))}
          {view.soloReview ? (
            <li>
              <span className="mission-tag warn" title="找不到第二個可用身分，改由執行者自審＋驗證者把關">
                無獨立 reviewer
              </span>
            </li>
          ) : null}
        </ul>
      ) : null}

      {/* 每一件交辦的 role 與狀態（P1b）。角色那一列講的是「現在誰在跑」，這裡講的是
          「一路上派了哪幾件、各自到哪了」——撞限換手在這裡看得到是兩件而不是一件。 */}
      {view.assignments.length > 0 ? (
        <ul className="mission-asgs">
          {view.assignments.map((a) => (
            <li key={a.id} className={assignmentOpen(a) ? 'open' : ''}>
              <span className="mission-role">{a.role ? ROLE_LABEL[a.role] : '—'}</span>
              <span className={`mission-asg-state${a.turn_error || a.turn_status === 'quota_exhausted' ? ' warn' : ''}`}>
                {assignmentLabel(a)}
              </span>
              {a.target_bot_id ? <span className="mission-who">{a.target_bot_id}</span> : null}
              {a.follow_up_of ? <span className="mission-tag">接手</span> : null}
              {a.turn_error ? (
                <span className="mission-asg-err" title={a.turn_error}>
                  {a.turn_error}
                </span>
              ) : null}
            </li>
          ))}
        </ul>
      ) : null}

      {view.handoffs.length > 0 ? (
        <ul className="mission-handoffs">
          {view.handoffs.map((h, i) => (
            <li key={`${h.at}-${i}`}>
              <span className="mission-tag warn">撞限換手</span>
              {h.from ?? '?'} → {h.to ?? '?'}
              {h.model ? ` · ${h.model}` : ''}
              {h.reason && h.reason !== 'limit_hit' ? `（${h.reason}）` : ''}
            </li>
          ))}
        </ul>
      ) : null}

      <Evidence view={view} mission={mission} />

      {paused && view.ask ? (
        <div className="mission-ask">
          <p className="mission-ask-why">
            <strong>{pausedLabel(view.ask.reason)}</strong>
            {view.ask.detail ? `：${view.ask.detail}` : ''}
          </p>
          {view.ask.question ? <p className="mission-ask-q">{view.ask.question}</p> : null}
          {view.ask.resets.length > 0 ? (
            <ul className="mission-resets">
              {view.ask.resets.map((r) => (
                <li key={r.identity}>
                  {r.identity} Fable 額度 {shortTime(r.resets_at)} 回來
                </li>
              ))}
            </ul>
          ) : null}
          <div className="mission-ask-row">
            <textarea
              className="mission-answer"
              rows={2}
              value={answer}
              placeholder="回答 AGM，送出後任務就會繼續"
              disabled={sending}
              onChange={(e) => setAnswer(e.target.value)}
            />
            <button
              type="button"
              className="btn primary mission-send"
              disabled={sending || !answer.trim()}
              onClick={() => {
                setSending(true)
                void answerMission(mission.id, answer.trim()).then((ok) => {
                  setSending(false)
                  if (ok) setAnswer('')
                })
              }}
            >
              {sending ? '送出中…' : '回答並繼續'}
            </button>
          </div>
        </div>
      ) : view.phase === 'awaiting_agm' || view.phase === 'waiting_quota' ? (
        /* 「等 AGM」「等額度」不用使用者動手，但它自己不會動——要看得出來這是在等，
           不是在跑（AGM 2026-09-13：awaiting_agm／paused 要有明顯提示）。 */
        <div className="mission-wait" role="status">
          <strong>{view.phase === 'waiting_quota' ? '等額度重置' : '等 AGM 決定下一步'}</strong>
          <span>
            {view.phase === 'waiting_quota'
              ? quotaWaitText(mission, [...view.assignments].reverse().find((a) => a.status === 'quota_blocked') ?? null, shortTime)
              : '這一輪的交辦都結案了，AGM 還沒派下一件。它醒來就會接著跑，不需要你動手。'}
          </span>
          {view.latest?.text ? <span className="mission-wait-last">最後一則：{view.latest.text}</span> : null}
        </div>
      ) : (
        <p className="mission-latest">{view.latest?.text ?? '等 AGM 接手…'}</p>
      )}

      <div className="mission-actions">
        {paused ? (
          <button type="button" className="mini-btn" onClick={() => void controlMission(mission.id, 'resume')}>
            不回答，直接繼續
          </button>
        ) : (
          <button type="button" className="mini-btn" onClick={() => void controlMission(mission.id, 'pause')}>
            暫停
          </button>
        )}
        <button
          type="button"
          className="mini-btn danger"
          onClick={() => {
            if (confirm(`取消任務「${mission.text}」？已經做的不會回復。`)) void controlMission(mission.id, 'cancel')
          }}
        >
          取消任務
        </button>
      </div>
    </article>
  )
}

function shortTime(iso: string | null): string {
  if (!iso) return ''
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return ''
  return `${d.getMonth() + 1}/${d.getDate()} ${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}
