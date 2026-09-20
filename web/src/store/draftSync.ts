/**
 * 別的分頁把草稿清掉（多半是它送出去了）：這個分頁若還留著同一段字，使用者會再送一次（#369）。
 * 只處理「對方刪了某個鍵、而這裡的值跟對方刪之前一樣（沒動過）」；這裡改過的字是使用者正在打的，不動。
 * 對方新增／改寫的草稿不套過來：兩邊同時打同一顆 bot 時，各留各的比互相蓋掉安全。
 */
export function draftsClearedElsewhere(
  local: Record<string, string>,
  oldRaw: string | null,
  newRaw: string | null,
): string[] {
  const parse = (raw: string | null): Record<string, unknown> => {
    try {
      const v: unknown = raw ? JSON.parse(raw) : {}
      return v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {}
    } catch {
      return {}
    }
  }
  const before = parse(oldRaw)
  const after = parse(newRaw)
  return Object.keys(before).filter((k) => !(k in after) && typeof before[k] === 'string' && local[k] === before[k])
}
