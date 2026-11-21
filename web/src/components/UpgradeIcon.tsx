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
