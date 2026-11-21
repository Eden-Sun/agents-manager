import { useState } from 'react'
import type { Project } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { useSavePart } from './projectSaveGroup'

export function ProjectSpecialistDispatchEditor({ project }: { project: Project }) {
  const t = useT()
  const patchProject = useStore((s) => s.patchProject)
  const saved = project.specialist_dispatch
  const dispatchKey = `${project.id}:${saved}`
  const [prevKey, setPrevKey] = useState(dispatchKey)
  const [on, setOn] = useState(saved ?? true)
  const [saving, setSaving] = useState(false)

  if (dispatchKey !== prevKey) {
    setPrevKey(dispatchKey)
    setOn(saved ?? true)
  }

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
