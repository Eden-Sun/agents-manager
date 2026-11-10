/** Shared line icons for the chrome: stroked SVGs, since ⚙ / ⋯ glyphs vanish against a selected row at sidebar sizes. */

export function GearIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <circle cx="8" cy="8" r="2.35" fill="none" stroke="currentColor" strokeWidth="1.6" />
      <path
        d="M8 1.4l.62 1.63a5 5 0 011.28.53l1.6-.7 1.64 1.64-.7 1.6a5 5 0 01.53 1.28l1.63.62v2.32l-1.63.62a5 5 0 01-.53 1.28l.7 1.6-1.64 1.64-1.6-.7a5 5 0 01-1.28.53L8 14.6l-.62-1.63a5 5 0 01-1.28-.53l-1.6.7-1.64-1.64.7-1.6a5 5 0 01-.53-1.28L1.4 8.68V6.36l1.63-.62a5 5 0 01.53-1.28l-.7-1.6L4.5 1.22l1.6.7a5 5 0 011.28-.53z"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.35"
        strokeLinejoin="round"
      />
    </svg>
  )
}

/** Git 的菱形分支標誌，用於收合的專案資訊入口。 */
export function GitIcon() {
  return (
    <svg viewBox="0 0 24 24" width="1em" height="1em" aria-hidden="true">
      <rect x="3.5" y="3.5" width="17" height="17" rx="2" transform="rotate(45 12 12)" fill="currentColor" />
      <path d="M9 5v11M9 8l6 6" stroke="var(--bg-panel)" strokeWidth="1.8" fill="none" />
      <g fill="var(--bg-panel)"><circle cx="9" cy="8" r="2" /><circle cx="9" cy="17" r="2" /><circle cx="15" cy="14" r="2" /></g>
    </svg>
  )
}

/** 溢位選單（`⋯`）：把不常按、又不該常駐在標題列上的動作收起來。 */
export function MoreIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <circle cx="3.4" cy="8" r="1.35" fill="currentColor" />
      <circle cx="8" cy="8" r="1.35" fill="currentColor" />
      <circle cx="12.6" cy="8" r="1.35" fill="currentColor" />
    </svg>
  )
}

/** 時鐘圖示：取代 emoji ⏱，避免 headless 環境缺字型顯示成 tofu 方塊。 */
export function ClockIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" strokeWidth="1.4" />
      <path d="M8 4.5v3.8l2.6 1.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

/** Bot 機器人圖示：取代 emoji 🤖，避免 headless 環境缺字型顯示成 tofu 方塊。 */
export function BotIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <rect x="2.5" y="4.5" width="11" height="8" rx="2" fill="none" stroke="currentColor" strokeWidth="1.3" />
      <path d="M8 2v2.5M1 8.5h1.5M13.5 8.5H15" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
      <circle cx="5.5" cy="8.5" r="1" fill="currentColor" />
      <circle cx="10.5" cy="8.5" r="1" fill="currentColor" />
    </svg>
  )
}

/** 禁止派工：圓圈加斜線，跟其他 icon 同一套線寬。 */
export function NoDispatchIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <circle cx="8" cy="8" r="5.25" fill="none" stroke="currentColor" strokeWidth="1.6" />
      <path d="M4.3 11.7l7.4-7.4" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
    </svg>
  )
}

/** 提示符加底線：終端的通用符號，跟其他 icon 同一套線寬。 */
export function TerminalIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <path
        d="M3.5 4.5l3 3-3 3M8.5 11.5h4"
        stroke="currentColor"
        strokeWidth="1.6"
        fill="none"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  )
}

