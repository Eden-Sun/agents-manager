import { useStore } from '../store/store'
import { t, useT } from '../lib/i18n'
import { humanBytes } from './memFormat'
import './projectMemBadge.css'

/**
 * 側欄專案標題上的「幾個 pane · 多少 RAM」（使用者 2026-09-15）。算的是這個專案的 bot 與它們的 child
 * 在 pane 裡跑的程序（`GET /api/mem` 的 `projects`）；沒有在跑的專案不畫，量不到也不畫成 0。
 */
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
