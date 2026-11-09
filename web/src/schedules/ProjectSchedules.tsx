import { useCallback, useEffect, useRef, useState } from 'react'
import { listSchedules, rebindSchedules, type Schedule } from '../api/schedules'
import { useStore } from '../store/store'
import { scheduleStatusLine, scheduleStatusShort } from './format'
import { formatDuration } from './duration'
import { useLiveTurnStatus, useOnTurnChange } from './liveTurn'
import { ScheduleRowMenu } from './ScheduleRowMenu'
import './schedules.css'
import { t, useLang } from '../lib/i18n'

function ScheduleRow({ s, onOpen, onTurnChange, selecting, selected, onSelect }: { s: Schedule; onOpen: (id: string, focusBot?: boolean) => void; onTurnChange: () => void; selecting: boolean; selected: boolean; onSelect: (id: string) => void }) {
  const live = useLiveTurnStatus(s.open?.bot_id, s.open?.turn_id)
  const botName = useStore((st) => st.bots.find((b) => b.id === s.bot_id)?.name)
  const selectBot = useStore((st) => st.selectBot)
  useOnTurnChange(live, onTurnChange)
  return (
    <li className={`project-schedule-item${s.enabled === false ? ' off' : ''}`}>
      {selecting ? <input type="checkbox" aria-label={t('選取排程 {name}', { name: s.name })} checked={selected} onChange={() => onSelect(s.id)} /> : null}
      {/* 一行：名稱靠左、短狀態靠右；完整時間（含時區）放 title。 */}
      <button type="button" className="project-schedule-row" title={`${s.name} — ${scheduleStatusLine(s, live)}`} onClick={() => onOpen(s.id, s.bot_available === false)}>
        <span className="project-schedule-name">⏱ {s.code ? <span className="project-schedule-code">{s.code} </span> : null}{s.name}</span>
        {s.last?.duration_ms != null ? <span className="project-schedule-last" title={t('上次執行總花費時間')}>{t('上次 {d}', { d: formatDuration(s.last.duration_ms) })}</span> : null}
        <span className={`project-schedule-status${s.bot_available === false ? ' needs-rebind' : ''}`}>{s.bot_available === false ? t('⚠ 需要重新指定 Bot') : scheduleStatusShort(s, live)}</span>
        {s.bot_available !== false && s.bot_dedicated === false ? <span className="project-schedule-status needs-rebind" title={t('這個排程綁的不是排程專屬 bot：照常執行；開啟編輯可轉成專屬或改綁到新專屬 bot')}>{t('非專屬 bot')}</span> : null}
      </button>
      {s.bot_available !== false && botName ? (
        <button type="button" className="project-schedule-bot" title={t('跳到這個排程綁定的 Bot：{name}', { name: botName })} aria-label={t('跳到 Bot {name}', { name: botName })} onClick={() => selectBot(s.bot_id)}>
          🤖 {botName}
        </button>
      ) : null}
      <ScheduleRowMenu schedule={s} onOpen={onOpen} />
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
      if (detail?.project_id === projectId) refresh()
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
