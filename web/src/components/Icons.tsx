/**
 * Shared line icons for the chrome (settings, row menu).
 *
 * These used to be the ⚙ / ⋯ characters, which render as hairline glyphs at the sizes the
 * sidebar uses and all but disappear against a selected row. A stroked SVG at
 * `currentColor` keeps the same weight as the rest of the UI's icons and stays legible.
 */

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

/** 「開同類分身」：兩張疊在一起的卡片。 */
export function CloneIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <rect x="2.2" y="2.2" width="8" height="8" rx="1.8" fill="none" stroke="currentColor" strokeWidth="1.4" />
      <path
        d="M5.8 13.8h6.2a1.8 1.8 0 001.8-1.8V5.8"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
      />
    </svg>
  )
}

export function PlayIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <path d="M5 3.4l7 4.6-7 4.6z" fill="currentColor" />
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

export function TrashIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <path
        d="M3 4.6h10M6.3 4.6V3.1a.9.9 0 01.9-.9h1.6a.9.9 0 01.9.9v1.5M6.7 7.4v4.4M9.3 7.4v4.4"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
      />
      <path
        d="M4.1 4.6l.6 8.1a1.6 1.6 0 001.6 1.5h3.4a1.6 1.6 0 001.6-1.5l.6-8.1"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinejoin="round"
      />
    </svg>
  )
}
