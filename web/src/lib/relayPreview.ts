
export const RELAY_PREVIEW_MAX = 90

export interface RelayPreview {
  text: string
  truncated: boolean
  length: number
}

export function relayPreview(content: string): RelayPreview {
  const full = content.trim()
  const length = [...full].length
  const first = full.split('\n').find((l) => l.trim() !== '')?.trim() ?? ''
  const chars = [...first]
  const clipped = chars.length > RELAY_PREVIEW_MAX
  const text = clipped ? `${chars.slice(0, RELAY_PREVIEW_MAX).join('')}…` : first
  return { text, truncated: clipped || first !== full, length }
}
