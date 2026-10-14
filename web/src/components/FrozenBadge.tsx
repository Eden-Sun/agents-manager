import './keepWarmChip.css'

/**
 * 不用保溫（冰凍）的主力：雪花（inline SVG，不用 emoji，#1180）。畫在主力晶片與側欄列的名字旁；
 * 文字說明放在外層的 `title`、晶片的 sr-only 與狀態卡（SPEC §6.5k）。
 */
export function FrozenBadge({ title }: { title?: string }) {
  return (
    <span className="frozen-badge" aria-hidden="true" title={title}>
      <svg viewBox="0 0 12 12" width="11" height="11" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round">
        <path d="M6 1v10M1.8 3.5l8.4 5M10.2 3.5l-8.4 5M4.6 1.8L6 3l1.4-1.2M4.6 10.2L6 9l1.4 1.2" />
      </svg>
    </span>
  )
}
