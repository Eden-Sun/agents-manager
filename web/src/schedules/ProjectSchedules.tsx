import { useCallback, useEffect, useRef, useState } from 'react'
import { listSchedules, rebindSchedules, type Schedule } from '../api/schedules'
import { useStore } from '../store/store'
import { scheduleRowLines } from './format'
import { useNowMs } from './duration'
import { useLiveTurnStatus, useOnTurnChange } from './liveTurn'
import { ScheduleRowMenu } from './ScheduleRowMenu'
import { BotIcon } from '../components/Icons'
import './schedules.css'
import { t, useLang } from '../lib/i18n'

function ScheduleRow({ s, onOpen, onTurnChange, selecting, selected, onSelect }: { s: Schedule; onOpen: (id: string, focusBot?: boolean) => void; onTurnChange: () => void; selecting: boolean; selected: boolean; onSelect: (id: string) => void }) {
  const live = useLiveTurnStatus(s.open?.bot_id, s.open?.turn_id)
  const botName = useStore((st) => st.bots.find((b) => b.id === s.bot_id)?.name)
  const selectBot = useStore((st) => st.selectBot)
  useOnTurnChange(live, onTurnChange)
  // 執行中才每秒重畫（已進行時間會走）；沒有執行中就不開計時器。
  const running = !!s.open && (!!s.open.running_since || (live ?? s.open.turn_status) === 'in_flight')
  const nowMs = useNowMs(running)
  const { dot, next, last } = scheduleRowLines(s, live, nowMs)
  const dedicatedNote = s.bot_dedicated === false ? t('非專屬 bot：照常執行；開啟編輯可轉成專屬或改綁到新專屬 bot') : null
  return (
    <li className={`project-schedule-item${s.enabled === false ? ' off' : ''}`}>
      {selecting ? <input type="checkbox" aria-label={t('選取排程 {name}', { name: s.name })} checked={selected} onChange={() => onSelect(s.id)} /> : null}
      {/* 固定三行：名稱（狀態點）／下次或現在／上次。每行不換行，過長省略號，完整文字放 title。 */}
      <button type="button" className="project-schedule-row" title={s.name} onClick={() => onOpen(s.id, s.bot_available === false)}>
        <span className="project-schedule-head">
          <span className={`project-schedule-dot ${dot}`} aria-hidden="true" />
          {s.code ? <span className="project-schedule-code">{s.code}</span> : null}
          <span className="project-schedule-name">{s.name}</span>
        </span>
        <span className={`project-schedule-fact${next.tone ? ' needs-rebind' : ''}`} title={next.title}>
          <span className="project-schedule-k">{next.label ?? ''}</span>
          <span className="project-schedule-v">{next.text}</span>
        </span>
        <span className="project-schedule-fact" title={last.title}>
          <span className="project-schedule-k">{last.label}</span>
          <span className="project-schedule-v">{last.text}</span>
        </span>
      </button>
      <div className="project-schedule-side">
        {s.bot_available !== false && botName ? (
          <button type="button" className={`project-schedule-bot${s.bot_dedicated === false ? ' warn' : ''}`} title={dedicatedNote ? `${t('執行這個排程的 Bot')}\n${dedicatedNote}` : t('執行這個排程的 Bot')} aria-label={t('跳到 Bot {name}', { name: botName })} onClick={() => selectBot(s.bot_id)}>
            <BotIcon /> {botName}
          </button>
        ) : null}
        <ScheduleRowMenu schedule={s} onOpen={onOpen} onChanged={onTurnChange} />
      </div>
    </li>
  )
}

/** 專案 menu 裡的排程列表：下次時間（附時區）、本次狀態（含綁定 turn 的即時狀態），點開進編輯／歷史。 */
export function ProjectSchedules({ projectId, onOpen }: { projectId: string; onOpen: (scheduleId: string, focusBot?: boolean) => void }) {
  // 切語言時重繪；字串在 render 與 callback 裡直接用模組的 t（callback 不會卡在舊語言）。
  useLang()
  const schedulesRev = useStore((s) => s.schedulesRev)
  const [rows, setRows] = useState<Schedule[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [selecting, setSelecting] = useState(false)
  const [selected, setSelected] = useState<string[]>([])
  const [target, setTarget] = useState('')
  const [saving, setSaving] = useState(false)
  const [batchError, setBatchError] = useState<string | null>(null)
  const bots = useStore((s) => s.bots)
  const eligible = bots.filter((b) => b.project_id === projectId && b.managed_by === 'user' && !b.parent_bot_id && !b.share_profile)
  const requestSequence = useRef(0)

  const refresh = useCallback(() => {
    const requestId = ++requestSequence.current
    listSchedules(projectId)
      .then((r) => {
        if (requestSequence.current !== requestId) return
        setRows(r)
        setError(null)
      })
      .catch((e: unknown) => {
        if (requestSequence.current === requestId) setError(e instanceof Error ? e.message : t('讀取排程失敗'))
      })
  }, [projectId])

  useEffect(() => {
    refresh()
    return () => {
      requestSequence.current += 1
    }
  }, [refresh, schedulesRev, bots])

  useEffect(() => {
    // WS `schedules_changed` 只帶 id，這裡收到同專案的事件就重抓，不把 prompt 放進全域 state。
    const onChanged = (ev: Event) => {
      const detail = (ev as CustomEvent<{ project_id?: string }>).detail
      if (!detail?.project_id || detail.project_id === projectId) refresh()
    }
    window.addEventListener('agm:schedules_changed', onChanged)
    return () => window.removeEventListener('agm:schedules_changed', onChanged)
  }, [projectId, refresh])

  if (rows === null && error === null) return null
  if (error) return <p className="hint schedule-error">{t('排程讀取失敗：{reason}', { reason: error })}</p>
  if (!rows || rows.length === 0) return null

  return (
    <div className="project-schedules-wrap">
    <ul className="project-schedules" aria-label={t('排程')}>
      {rows.map((s) => (
        <ScheduleRow key={s.id} s={s} onOpen={onOpen} onTurnChange={refresh} selecting={selecting} selected={selected.includes(s.id)} onSelect={(id) => setSelected((prev) => prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id])} />
      ))}
    </ul>
    {!selecting ? <button type="button" className="btn" onClick={() => setSelecting(true)}>{t('批次改綁 Bot')}</button> : <div className="schedule-batch">
      <label>{t('改綁到 Bot')} <select value={target} onChange={(e) => setTarget(e.target.value)} disabled={saving}>
        <option value="">{t('請選擇 Bot')}</option>
        {eligible.map((b) => <option key={b.id} value={b.id}>{b.name}</option>)}
      </select></label>
      <button type="button" className="btn" disabled={saving || !target || !selected.length} onClick={() => {
        setSaving(true); setBatchError(null)
        void rebindSchedules(projectId, selected, target).then(() => { setSelected([]); setSelecting(false); refresh() }).catch((e: unknown) => setBatchError(e instanceof Error ? e.message : t('操作失敗'))).finally(() => setSaving(false))
      }}>{t('改綁所選排程')}</button>
      <button type="button" className="btn" disabled={saving} onClick={() => { setSelecting(false); setSelected([]); setBatchError(null) }}>{t('取消')}</button>
      {batchError ? <p role="alert">{batchError}</p> : null}
    </div>}
    </div>
  )
}
