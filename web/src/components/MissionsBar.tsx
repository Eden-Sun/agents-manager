/**
 * 群組裡的任務條（`docs/goals/agm-missions.md` §5）：進行中的任務卡，加上一份可以展開的
 * 「已完成任務」清單。
 *
 * 位置刻意放在**輸入框正上方**而不是訊息列上方：任務卡是要動手的東西——停下來問人的時候
 * 要在這裡回答——跟 IssuesBar 那種「這個專案有什麼可以挑」不一樣，捲到哪裡都不該找不到它。
 */
import { useEffect, useMemo, useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Mission } from '../api/types'
import { deliveryLabel, missionView, pausedLabel, phaseLabel, MISSION_PHASES, PHASE_LABEL, ROLE_LABEL } from '../lib/missionView'
import { useStore } from '../store/store'
import { KindIcon } from './KindTag'
import './missions.css'

/** 一次最多攤開幾張進行中的卡；其餘收在「還有 N 個」後面。 */
const OPEN_SHOWN = 2

export function MissionsBar({ projectId }: { projectId: string }) {
  const supported = useStore((s) => s.missionsSupported)
  const missions = useStore(useShallow((s) => s.missions[projectId] ?? []))
  const loadMissions = useStore((s) => s.loadMissions)

  useEffect(() => {
    if (supported) void loadMissions(projectId)
  }, [projectId, supported, loadMissions])

  const [showAll, setShowAll] = useState(false)
  const [showDone, setShowDone] = useState(false)

  const { open, done } = useMemo(() => {
    const open: Mission[] = []
    const done: Mission[] = []
    for (const m of missions) {
      if (m.status === 'done' || m.status === 'cancelled') done.push(m)
      else open.push(m)
    }
    return { open, done }
  }, [missions])

  if (!supported || (open.length === 0 && done.length === 0)) return null
  // 停下來問人的先排前面——那是唯一在等使用者的東西。
  const sorted = [...open].sort((a, b) => Number(b.status === 'paused') - Number(a.status === 'paused'))
  const shown = showAll ? sorted : sorted.slice(0, OPEN_SHOWN)
  const hidden = sorted.length - shown.length

  return (
    <section className="missions-bar" aria-label="群組任務">
      {shown.map((m) => (
        <MissionCard key={m.id} mission={m} />
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
            已完成任務（{done.length}）
          </button>
          {showDone ? (
            <ul className="mission-done-list">
              {done.map((m) => (
                <MissionDoneRow key={m.id} mission={m} />
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
function MissionDoneRow({ mission }: { mission: Mission }) {
  const detail = useStore((s) => s.missionDetail[mission.id] ?? null)
  const loadMission = useStore((s) => s.loadMission)
  const [open, setOpen] = useState(false)
  const view = missionView(mission, detail?.events ?? [], detail?.assignments ?? [])

  return (
    <li className={`mission-done-row${mission.status === 'cancelled' ? ' cancelled' : ''}`}>
      <button
        type="button"
        className="mission-done-row-head"
        aria-expanded={open}
        onClick={() => {
          setOpen((v) => !v)
          if (!detail) void loadMission(mission.id)
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
          {!detail ? <p className="mission-hint">讀取中…</p> : null}
        </div>
      ) : null}
    </li>
  )
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
function MissionCard({ mission }: { mission: Mission }) {
  const detail = useStore((s) => s.missionDetail[mission.id] ?? null)
  const loadMission = useStore((s) => s.loadMission)
  const controlMission = useStore((s) => s.controlMission)
  const answerMission = useStore((s) => s.answerMission)
  const [answer, setAnswer] = useState('')
  const [sending, setSending] = useState(false)

  // 進行中的卡一定要有事件串才畫得出進度與角色，進來就抓一次；之後靠 WS `mission_updated`。
  useEffect(() => {
    if (!detail) void loadMission(mission.id)
  }, [mission.id, detail, loadMission])

  const view = missionView(mission, detail?.events ?? [], detail?.assignments ?? [])
  // 「在等人」的三種都用同一種醒目底色：等你回答、等額度、等 AGM——共同點是它自己不會動。
  const paused = view.phase === 'paused'
  const waiting = paused || view.phase === 'waiting_quota' || view.phase === 'awaiting_agm'

  return (
    <article className={`mission-card${waiting ? ' paused' : ''}`}>
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

      {view.handoffs.length > 0 ? (
        <ul className="mission-handoffs">
          {view.handoffs.map((h, i) => (
            <li key={`${h.at}-${i}`}>
              <span className="mission-tag warn">撞限換手</span>
              {h.from ?? '?'} → {h.to ?? '?'}
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
