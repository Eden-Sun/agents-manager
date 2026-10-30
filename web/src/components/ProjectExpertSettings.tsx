import { useState } from 'react'
import { useStore } from '../store/store'

export function ProjectExpertSettings({ projectId }: { projectId: string }) {
  const project = useStore((s) => s.projects.find((p) => p.id === projectId))
  const allBots = useStore((s) => s.bots)
  const bots = allBots.filter((b) => b.project_id === projectId && b.managed_by !== 'child')
  const patchProject = useStore((s) => s.patchProject)
  const [manager, setManager] = useState('')
  if (!project) return null
  const selected = manager || bots.find((b) => b.role === 'manager')?.id || ''
  return <details><summary>專案模式設定</summary>
    <label>管理者 <select value={selected} onChange={(e) => setManager(e.target.value)}>
      <option value="">請選擇</option>{bots.map((b) => <option key={b.id} value={b.id}>{b.name}</option>)}
    </select></label>
    <button type="button" disabled={!selected} onClick={() => void patchProject(projectId, { mode: 'expert', manager_bot_id: selected })}>套用專家模式</button>
    <button type="button" onClick={() => void patchProject(projectId, { mode: 'parent_child' })}>套用 parent-child 模式</button>
    <span>目前設定：{project.mode ?? 'parent_child'}；建議 manager 使用 Opus、executor 使用 Haiku、advisor 使用 Astra。</span>
  </details>
}
