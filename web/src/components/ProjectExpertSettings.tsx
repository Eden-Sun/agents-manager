import { useState } from 'react'
import { BOT_KINDS, type BotKind } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'

export function ProjectExpertSettings({ projectId }: { projectId: string }) {
  const t = useT()
  const project = useStore((s) => s.projects.find((p) => p.id === projectId))
  const allBots = useStore((s) => s.bots)
  const bots = allBots.filter((b) => b.project_id === projectId && b.managed_by !== 'child')
  const patchProject = useStore((s) => s.patchProject)
  const [manager, setManager] = useState('')
  const [poolInput, setPoolInput] = useState<Record<string, number> | null>(null)
  const [savingPool, setSavingPool] = useState(false)

  if (!project) return null
  const selected = manager || bots.find((b) => b.role === 'manager')?.id || ''
  const isExpert = project.mode === 'expert'

  const currentPool = poolInput ?? project.executor_pool ?? {}

  const handleKindCountChange = (kind: BotKind, valStr: string) => {
    const val = parseInt(valStr, 10)
    const n = isNaN(val) ? 0 : Math.max(0, Math.min(10, val))
    setPoolInput({
      ...currentPool,
      [kind]: n,
    })
  }

  const handleSavePool = async () => {
    setSavingPool(true)
    const toSend: Record<string, number> = {}
    for (const kind of BOT_KINDS) {
      const count = currentPool[kind] ?? 0
      if (count > 0) {
        toSend[kind] = count
      }
    }
    await patchProject(projectId, { executor_pool: toSend })
    setSavingPool(false)
    setPoolInput(null)
  }

  return (
    <details open className="project-expert-settings-details">
      <summary>{t('專案模式設定')}</summary>
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

        {isExpert ? (
          <div className="project-executor-pool-section">
            <h4>{t('執行者池')}</h4>
            <p className="hint">
              {t('管理者增聘執行者時只能用這裡開放的 kind 與數量；全部 0＝不能增聘')}
            </p>
            <div className="project-executor-pool-grid">
              {BOT_KINDS.map((kind) => {
                const count = currentPool[kind] ?? 0
                return (
                  <label key={kind} className="project-executor-pool-row">
                    <span className="executor-pool-kind">{kind}</span>
                    <input
                      type="number"
                      min={0}
                      max={10}
                      value={count}
                      onChange={(e) => handleKindCountChange(kind, e.target.value)}
                      className="executor-pool-input"
                    />
                  </label>
                )
              })}
            </div>
            <div className="project-executor-pool-actions">
              <button
                type="button"
                className="btn"
                disabled={savingPool || poolInput === null}
                onClick={() => void handleSavePool()}
              >
                {savingPool ? t('儲存中…') : t('儲存執行者池')}
              </button>
            </div>
          </div>
        ) : null}
      </div>
    </details>
  )
}
