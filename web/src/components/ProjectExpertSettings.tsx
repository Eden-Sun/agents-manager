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

  if (!project) return null
  const selected = manager || bots.find((b) => b.role === 'manager')?.id || ''
  const isExpert = project.mode === 'expert'

  return (
    <div className="project-expert-settings-section">
      <h3>{t('專案模式設定')}</h3>
      <div className="project-expert-settings-body">
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
        <div className="project-expert-settings-actions">
          <button
            type="button"
            className="btn"
            disabled={!selected}
            onClick={() => void patchProject(projectId, { mode: 'expert', manager_bot_id: selected })}
          >
            {t('套用專家模式')}
          </button>
          <button type="button" className="btn" onClick={() => void patchProject(projectId, { mode: 'parent_child' })}>
            {t('套用 parent-child 模式')}
          </button>
        </div>
        <p className="hint">
          {t('目前設定：{mode}；建議 manager 使用 Opus、executor 使用 Haiku、advisor 使用 Astra。', {
            mode: project.mode ?? 'parent_child',
          })}
        </p>

        {isExpert ? <ExecutorPoolEditor project={project} bots={bots} /> : null}
        {isExpert ? <ProjectDeputyEditor project={project} bots={bots} /> : null}
        {isExpert ? <RolePolicyEditor project={project} /> : null}
      </div>
    </div>
  )
}
