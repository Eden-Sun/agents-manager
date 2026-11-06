import { useEffect, useState } from 'react'
import type { Project } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { useSavePart } from './projectSaveGroup'

/**
 * 專案設定的「專家可直接派工給執行者」總開關（使用者 2026-11-06，取代逐對授權）：開著時本專案的專家可直接派工給任何執行者
 * （含群集）並驗收；關掉＝只有管理者（或代班中的副管理者）能派。舊 daemon 沒有這個欄位（`specialist_dispatch` 為 undefined）就不顯示。
 * 只存草稿，按「儲存」才送 PATCH（User-only）。
 */
export function ProjectSpecialistDispatchEditor({ project }: { project: Project }) {
  const t = useT()
  const patchProject = useStore((s) => s.patchProject)
  const saved = project.specialist_dispatch
  const [on, setOn] = useState(saved ?? true)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    setOn(saved ?? true)
  }, [saved, project.id])

  const supported = saved !== undefined
  const dirty = supported && on !== saved
  const group = useSavePart('specialist_dispatch', { label: t('專家派工'), dirty, valid: true }, {
    patch: () => ({ specialist_dispatch: on }),
    reset: () => setOn(saved ?? true),
  })
  if (!supported) return null

  const save = () => {
    if (!dirty || saving) return
    setSaving(true)
    void patchProject(project.id, { specialist_dispatch: on }).finally(() => setSaving(false))
  }

  return (
    <div className="project-specialist-dispatch-editor" aria-label={t('專家派工')}>
      <label>
        <input type="checkbox" checked={on} onChange={(e) => setOn(e.target.checked)} /> {t('專家可直接派工給執行者')}
      </label>
      <p className="hint">{t('開著時專家可直接派工給本專案的執行者並驗收結果（同時派給同一顆照佇列排隊）；關掉就只有管理者能派。')}</p>
      {group.grouped ? null : (
        <button type="button" className="btn" disabled={!dirty || saving} onClick={save}>
          {t('儲存')}
        </button>
      )}
    </div>
  )
}
