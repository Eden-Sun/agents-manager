import { HeadMoreMenu } from '../components/HeadMoreMenu'
import { useT } from '../lib/i18n'

export function ProjectAddMenu({ label, onBot, onSchedule }: { label: string; onBot: () => void; onSchedule: () => void }) {
  const t = useT()
  return (
    <HeadMoreMenu label={t('在 {label} 新增', { label })} trigger="＋" triggerClassName="icon-btn add icon-tip">
      <button type="button" className="head-menu-item" role="menuitem" onClick={onBot}>
        {t('新增 Bot')}
      </button>
      <button type="button" className="head-menu-item" role="menuitem" onClick={onSchedule}>
        {t('新增排程')}
      </button>
    </HeadMoreMenu>
  )
}
