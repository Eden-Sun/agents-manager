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

export function PlayIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <path d="M5 3.4l7 4.6-7 4.6z" fill="currentColor" />
    </svg>
  )
}

export function StopIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <rect x="4" y="4" width="8" height="8" rx="1.4" fill="currentColor" />
    </svg>
  )
}
