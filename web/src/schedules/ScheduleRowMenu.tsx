import { useEffect, useRef, useState } from 'react'
import { getSchedule, patchSchedule, type Schedule } from '../api/schedules'
import { t, useT } from '../lib/i18n'
import { HeadMoreMenu } from '../components/HeadMoreMenu'
import { useStore } from '../store/store'
import { ConfirmDialog } from '../components/ConfirmDialog'
import { ScheduleEditor } from './ScheduleEditor'

function errorReason(e: unknown): string {
  return e instanceof Error ? e.message : t('操作失敗')
}

/** 複製先取最新設定，開新建表單；只有使用者儲存後才建立副本。 */
export function ScheduleRowMenu({ schedule, onOpen, onChanged }: { schedule: Schedule; onOpen: (id: string) => void; onChanged: () => void }) {
  const t = useT()
  const notify = useStore((s) => s.notify)
  const [copyOf, setCopyOf] = useState<Schedule | null>(null)
  const [loading, setLoading] = useState(false)
  const [busy, setBusy] = useState(false)
  const [confirmPause, setConfirmPause] = useState(false)
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

  // 列上直接停用／啟用（與編輯器同一支 API）；停用先確認，和編輯器一致。
  const toggle = async () => {
    if (busy) return
    setBusy(true)
    try {
      await patchSchedule(schedule.id, { enabled: !schedule.enabled })
      onChanged()
    } catch (e) {
      notify('error', schedule.enabled ? t('停用排程失敗：{reason}', { reason: errorReason(e) }) : t('啟用排程失敗：{reason}', { reason: errorReason(e) }))
    } finally {
      setBusy(false)
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
        <button type="button" className="head-menu-item" role="menuitem" disabled={busy} onClick={() => (schedule.enabled ? setConfirmPause(true) : void toggle())}>
          {schedule.enabled ? t('停用') : t('啟用')}
        </button>
      </HeadMoreMenu>
      <ConfirmDialog
        open={confirmPause}
        title={t('停用排程')}
        body={<>{t('只會撤回還沒送出的排隊執行，已經在跑的會繼續。')}</>}
        confirmLabel={t('停用')}
        onConfirm={() => { setConfirmPause(false); void toggle() }}
        onCancel={() => setConfirmPause(false)}
      />
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
