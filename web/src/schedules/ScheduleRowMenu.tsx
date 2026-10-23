import { useEffect, useRef, useState } from 'react'
import { getSchedule, type Schedule } from '../api/schedules'
import { useT } from '../lib/i18n'
import { HeadMoreMenu } from '../components/HeadMoreMenu'
import { useStore } from '../store/store'
import { ScheduleEditor } from './ScheduleEditor'

/** 複製先取最新設定，開新建表單；只有使用者儲存後才建立副本。 */
export function ScheduleRowMenu({ schedule, onOpen }: { schedule: Schedule; onOpen: (id: string) => void }) {
  const t = useT()
  const notify = useStore((s) => s.notify)
  const [copyOf, setCopyOf] = useState<Schedule | null>(null)
  const [loading, setLoading] = useState(false)
  const inFlight = useRef(false)
  const requestSequence = useRef(0)
  useEffect(() => () => { requestSequence.current += 1 }, [])

  const copy = async () => {
    if (inFlight.current) return
    inFlight.current = true
    const requestId = ++requestSequence.current
    setLoading(true)
    try {
      const source = await getSchedule(schedule.id)
      if (requestSequence.current !== requestId) return
      setCopyOf(source)
    } catch (e) {
      if (requestSequence.current === requestId) notify('error', t('複製排程失敗：{reason}', { reason: e instanceof Error ? e.message : t('讀取失敗') }))
    } finally {
      if (requestSequence.current === requestId) {
        inFlight.current = false
        setLoading(false)
      }
    }
  }

  return (
    <div className="project-schedule-actions">
      <HeadMoreMenu label={t('{name} 的排程操作', { name: schedule.name })}>
        <button type="button" className="head-menu-item" role="menuitem" onClick={() => onOpen(schedule.id)}>
          {t('編輯排程…')}
        </button>
        <button type="button" className="head-menu-item" role="menuitem" disabled={loading} onClick={() => void copy()}>
          {loading ? t('讀取中…') : t('複製排程…')}
        </button>
      </HeadMoreMenu>
      {copyOf ? (
        <ScheduleEditor
          projectId={copyOf.project_id}
          scheduleId={null}
          copyOf={copyOf}
          onClose={() => setCopyOf(null)}
          onSelect={(id) => {
            setCopyOf(null)
            onOpen(id)
          }}
        />
      ) : null}
    </div>
  )
}
