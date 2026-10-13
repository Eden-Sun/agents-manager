import { useCallback, useEffect, useState } from 'react'
import { listSchedules, type Schedule } from '../api/schedules'
import { scheduleStatusLine } from './format'
import { useLiveTurnStatus, useOnTurnChange } from './liveTurn'
import './schedules.css'

function ScheduleRow({ s, onOpen, onTurnChange }: { s: Schedule; onOpen: (id: string) => void; onTurnChange: () => void }) {
  const live = useLiveTurnStatus(s.open?.bot_id, s.open?.turn_id)
  useOnTurnChange(live, onTurnChange)
  return (
    <li>
      <button type="button" className="project-schedule-row" onClick={() => onOpen(s.id)}>
        <span className="project-schedule-name">⏱ {s.name}</span>
        <span className="project-schedule-status">{scheduleStatusLine(s, live)}</span>
      </button>
    </li>
  )
}

/** 專案 menu 裡的排程列表：下次時間（附時區）、本次狀態（含綁定 turn 的即時狀態），點開進編輯／歷史。 */
export function ProjectSchedules({ projectId, onOpen }: { projectId: string; onOpen: (scheduleId: string) => void }) {
  const [rows, setRows] = useState<Schedule[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(() => {
    listSchedules(projectId)
      .then((r) => {
        setRows(r)
        setError(null)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : '讀取排程失敗'))
  }, [projectId])

  useEffect(() => {
    refresh()
    // WS `schedules_changed` 只帶 id，這裡收到同專案的事件就重抓，不把 prompt 放進全域 state。
    const onChanged = (ev: Event) => {
      const detail = (ev as CustomEvent<{ project_id?: string }>).detail
      if (detail?.project_id === projectId) refresh()
    }
    window.addEventListener('agm:schedules_changed', onChanged)
    return () => window.removeEventListener('agm:schedules_changed', onChanged)
  }, [projectId, refresh])

  if (rows === null && error === null) return null
  if (error) return <p className="hint schedule-error">排程讀取失敗：{error}</p>
  if (!rows || rows.length === 0) return null

  return (
    <ul className="project-schedules" aria-label="排程">
      {rows.map((s) => (
        <ScheduleRow key={s.id} s={s} onOpen={onOpen} onTurnChange={refresh} />
      ))}
    </ul>
  )
}
