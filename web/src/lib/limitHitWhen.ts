/** 被擋到什麼時候：今天只寫時分；不是今天帶月日。讀不懂回 null。 */
export function limitHitWhen(until: string | null | undefined, now: number = Date.now()): string | null {
  if (!until) return null
  const d = new Date(until)
  if (Number.isNaN(d.getTime())) return null
  const p = (n: number) => String(n).padStart(2, '0')
  const time = `${p(d.getHours())}:${p(d.getMinutes())}`
  const t = new Date(now)
  const sameDay = d.getFullYear() === t.getFullYear() && d.getMonth() === t.getMonth() && d.getDate() === t.getDate()
  return sameDay ? time : `${d.getMonth() + 1}/${d.getDate()} ${time}`
}
