/** Readable text colour for a GitHub label hex. */
export function labelStyle(color: string | null) {
  if (!color) return undefined
  const r = parseInt(color.slice(0, 2), 16)
  const g = parseInt(color.slice(2, 4), 16)
  const b = parseInt(color.slice(4, 6), 16)
  const lum = (0.299 * r + 0.587 * g + 0.114 * b) / 255
  return { background: `#${color}`, color: lum > 0.6 ? '#1b1e23' : '#fff', borderColor: 'transparent' }
}
