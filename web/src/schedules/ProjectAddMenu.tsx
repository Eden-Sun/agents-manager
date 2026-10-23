import { HeadMoreMenu } from '../components/HeadMoreMenu'
import { useT } from '../lib/i18n'

/**
 * 專案列上的「＋」：展開成「新增 Bot／新增排程」。沿用 HeadMoreMenu 的選單（鍵盤、點外面關閉、Escape、視窗碰撞定位），
 * 只換觸發鈕。排程建立後放進該專案的 menu（見 ProjectSchedules）。
 */
export function ProjectAddMenu({ label, onBot, onSchedule }: { label: string; onBot: () => void; onSchedule: () => void }) {
  const t = useT()
  return (
    // i18n-ignore：「＋」是符號
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
