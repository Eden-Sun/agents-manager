import { useState } from 'react'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { ExecutorPoolEditor } from './ExecutorPoolEditor'
import { ProjectDeputyEditor } from './ProjectDeputyEditor'
import { RolePolicyEditor } from './RolePolicyEditor'

export function ProjectExpertSettings({ projectId }: { projectId: string }) {
  const t = useT()
  const project = useStore((s) => s.projects.find((p) => p.id === projectId))
  const allBots = useStore((s) => s.bots)
  const bots = allBots.filter((b) => b.project_id === projectId && b.managed_by !== 'child')
  const patchProject = useStore((s) => s.patchProject)
  const [manager, setManager] = useState('')
  const [modeDraft, setModeDraft] = useState<'expert' | 'parent_child' | null>(null)

  if (!project) return null
  const currentManager = bots.find((b) => b.role === 'manager')?.id || ''
  const selected = manager || currentManager
  const isExpert = project.mode === 'expert'
  const mode = modeDraft ?? (isExpert ? 'expert' : 'parent_child')
  // 一個「套用」：模式或管理者跟現況不同才可按（2026-11-01 使用者：專案設定要 review & 優化）。
  const changed = mode !== (isExpert ? 'expert' : 'parent_child') || (mode === 'expert' && selected !== currentManager)
  const apply = () => {
    void patchProject(projectId, mode === 'expert' ? { mode: 'expert', manager_bot_id: selected } : { mode: 'parent_child' }).then(() => {
      setModeDraft(null)
      setManager('')
    })
  }

  return (
    <div className="project-expert-settings-section">
      <h3>{t('專案模式設定')}</h3>
      <div className="project-expert-settings-body">
        <div className="project-expert-settings-actions">
          <div className="opt-group" role="radiogroup" aria-label={t('模式')}>
            <button type="button" role="radio" aria-checked={mode === 'expert'} className={`opt${mode === 'expert' ? ' on' : ''}`} onClick={() => setModeDraft('expert')}>
              {t('專家模式')}
            </button>
            <button type="button" role="radio" aria-checked={mode === 'parent_child'} className={`opt${mode === 'parent_child' ? ' on' : ''}`} onClick={() => setModeDraft('parent_child')}>
              {t('parent-child 模式')}
            </button>
          </div>
          {mode === 'expert' ? (
            <label>
              {t('管理者')}{' '}
              <select value={selected} onChange={(e) => setManager(e.target.value)}>
                <option value="">{t('請選擇')}</option>
                {bots.map((b) => (
                  <option key={b.id} value={b.id}>
                    {b.name}
                  </option>
                ))}
              </select>
            </label>
          ) : null}
          {changed ? (
            <button type="button" className="btn primary" disabled={mode === 'expert' && !selected} onClick={apply}>
              {t('套用')}
            </button>
          ) : null}
        </div>
        {mode === 'expert' ? <p className="hint">{t('建議：管理者用 Opus、執行者用 Haiku、顧問用 Astra。')}</p> : null}

        {isExpert ? <ExecutorPoolEditor project={project} bots={bots} /> : null}
        {isExpert ? <ProjectDeputyEditor project={project} bots={bots} /> : null}
        {isExpert ? <RolePolicyEditor project={project} /> : null}
      </div>
    </div>
  )
}
