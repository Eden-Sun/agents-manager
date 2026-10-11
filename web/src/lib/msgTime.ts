const p = (n: number) => String(n).padStart(2, '0')

/** 今天只寫時分，不是今天帶月日，不同年再帶年。讀不懂回空字串。 */
export function msgTimeLabel(iso: string, now: number = Date.now()): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return ''
  const time = `${p(d.getHours())}:${p(d.getMinutes())}`
  const n = new Date(now)
  if (d.getFullYear() !== n.getFullYear()) return `${d.getFullYear()}/${d.getMonth() + 1}/${d.getDate()} ${time}`
  if (d.getMonth() !== n.getMonth() || d.getDate() !== n.getDate()) return `${d.getMonth() + 1}/${d.getDate()} ${time}`
  return time
}

/** tooltip 的完整時間，本地時區 `YYYY-MM-DD HH:mm:ss`；讀不懂就原樣顯示。 */
export function msgTimeTitle(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`
}
