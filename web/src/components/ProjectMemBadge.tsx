import { useStore } from '../store/store'
import { t, useT } from '../lib/i18n'
import { humanBytes } from './memFormat'
import './projectMemBadge.css'

export function ProjectMemBadge({ projectId }: { projectId: string }) {
  useT()
  const row = useStore((s) => s.mem?.projects.find((p) => p.project_id === projectId) ?? null)
  if (!row || row.panes === 0) return null
  return (
    <span
      className="project-mem"
      title={t('這個專案的 Bot（含子 agent）現在開著 {n} 個 pane，常駐記憶體共 {memory}\n每 15 秒更新一次。', { n: row.panes, memory: humanBytes(row.bytes) })}
    >
      <span className="project-mem-k">{row.panes} pane</span>
      <span className="project-mem-v">{humanBytes(row.bytes)}</span>
    </span>
  )
}
