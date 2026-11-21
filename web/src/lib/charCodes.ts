
export const ESC = String.fromCharCode(0x1b)

export function hasCodeIn(s: string, lo: number, hi: number): boolean {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i)
    if (c >= lo && c <= hi) return true
  }
  return false
}
