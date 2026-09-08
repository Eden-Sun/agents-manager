/** 瀏覽器 logo（12px，跟徽章字級同高）。畫成幾何簡圖就夠認，不用載原廠圖。 */
export function ChromeIcon() {
  return (
    <svg className="browser-icon" viewBox="0 0 24 24" width="12" height="12" aria-hidden="true">
      <circle cx="12" cy="12" r="11" fill="#fff" />
      <path d="M12 1a11 11 0 0 1 9.53 5.5H12a5.5 5.5 0 0 0-4.76 2.75L3.47 4.1A11 11 0 0 1 12 1z" fill="#db4437" />
      <path d="M3.47 4.1l3.77 5.15A5.5 5.5 0 0 0 9.5 16.5L5.3 21.2A11 11 0 0 1 3.47 4.1z" fill="#0f9d58" />
      <path d="M21.53 6.5A11 11 0 0 1 5.3 21.2l4.2-4.7A5.5 5.5 0 0 0 16.76 12l4.77-5.5z" fill="#f4b400" />
      <circle cx="12" cy="12" r="4" fill="#4285f4" />
    </svg>
  )
}

export function EgoIcon() {
  return (
    <svg className="browser-icon" viewBox="0 0 24 24" width="12" height="12" aria-hidden="true">
      <circle cx="12" cy="12" r="11" fill="#1c1f26" stroke="#8b93a7" strokeWidth="1.5" />
      <path d="M16.5 14.5a5 5 0 1 1 0-5H12" fill="none" stroke="#e6e9f0" strokeWidth="2.2" strokeLinecap="round" />
      <circle cx="17" cy="12" r="1.6" fill="#e6e9f0" />
    </svg>
  )
}

export function BrowserIcon({ name }: { name: string }) {
  if (name === 'Chrome') return <ChromeIcon />
  if (name === 'ego') return <EgoIcon />
  return <span className="browser-icon browser-icon-text">{name.slice(0, 1)}</span>
}
