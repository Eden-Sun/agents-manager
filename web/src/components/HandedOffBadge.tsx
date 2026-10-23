import { useStore } from '../store/store'
import { handedOffTo } from '../lib/handoff'
import { t, useT } from '../lib/i18n'
import './handedOffBadge.css'

/** #708：側欄專案標題上的「由 <host> 管理」。這個專案的 bot／pane 歸那台主機的 daemon，這裡只顯示最後的狀態。 */
export function HandedOffBadge({ projectId }: { projectId: string }) {
  useT()
  const host = useStore((s) => handedOffTo(s.projects, projectId))
  if (!host) return null
  return (
    <span className="project-handoff" title={t('這個專案已移交給 {host} 的 daemon：這裡不啟動、停止或送訊息，也不動它的 pane。\n要收回請清掉專案的 handed_off_to。', { host })}>
      {t('由 {host} 管理', { host })}
    </span>
  )
}
