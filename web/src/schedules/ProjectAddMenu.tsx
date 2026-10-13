import { HeadMoreMenu } from '../components/HeadMoreMenu'

/**
 * 專案列上的「＋」：展開成「新增 Bot／新增排程」。沿用 HeadMoreMenu 的選單（鍵盤、點外面關閉、Escape、視窗碰撞定位），
 * 只換觸發鈕。排程建立後放進該專案的 menu（見 ProjectSchedules）。
 */
export function ProjectAddMenu({ label, onBot, onSchedule }: { label: string; onBot: () => void; onSchedule: () => void }) {
  return (
    <HeadMoreMenu label={`在 ${label} 新增`} trigger="＋" triggerClassName="icon-btn add icon-tip">
      <button type="button" className="head-menu-item" role="menuitem" onClick={onBot}>
        新增 Bot
      </button>
      <button type="button" className="head-menu-item" role="menuitem" onClick={onSchedule}>
        新增排程
      </button>
    </HeadMoreMenu>
  )
}
