/** 「有新版可升級」的圖示：雙層向上的 chevron（2026-09-09 使用者指定這種符號，取代文字 ⬆）。
 *  用 currentColor，跟著所在 badge / chip 的字色走；大小用 CSS `.upgrade-icon` 或 `size` 控。 */
export function UpgradeIcon({ size = 12, className }: { size?: number; className?: string }) {
  return (
    <svg
      className={`upgrade-icon${className ? ` ${className}` : ''}`}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="3"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d="M5 11l7-7 7 7" />
      <path d="M5 19l7-7 7 7" />
    </svg>
  )
}
