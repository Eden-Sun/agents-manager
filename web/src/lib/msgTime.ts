const p = (n: number) => String(n).padStart(2, '0')

/** 今天只寫時分，不是今天帶月日，不同年再帶年。讀不懂回空字串。`now` 只看日期部分，傳當天任一時刻或當天的開始都可以。 */
export function msgTimeLabel(iso: string, now: number = Date.now()): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return ''
  const time = `${p(d.getHours())}:${p(d.getMinutes())}`
  const n = new Date(now)
  if (d.getFullYear() !== n.getFullYear()) return `${d.getFullYear()}/${d.getMonth() + 1}/${d.getDate()} ${time}`
  if (d.getMonth() !== n.getMonth() || d.getDate() !== n.getDate()) return `${d.getMonth() + 1}/${d.getDate()} ${time}`
  return time
}

/**
 * 任意時刻的 `M/D HH:mm`（任務完成時間這類不分今天的欄位）：同年只寫月日，跨年才帶年。
 * 不加年份時，1 月的 `1/1 00:10` 會被讀成今年已經過去的那一天。讀不懂回空字串。
 */
export function dateTimeLabel(iso: string, now: number = Date.now()): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return ''
  const time = `${p(d.getHours())}:${p(d.getMinutes())}`
  const n = new Date(now)
  const head = d.getFullYear() !== n.getFullYear() ? `${d.getFullYear()}/` : ''
  return `${head}${d.getMonth() + 1}/${d.getDate()} ${time}`
}

/** 本地時區「今天」的開始（0 點）。跨日時它會變，拿來當 [`msgTimeLabel`] 的 `now`，標籤才會跟著換天。 */
export function startOfLocalDay(now: number): number {
  const d = new Date(now)
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime()
}

/** 下一個本地午夜（嚴格在 `now` 之後）。 */
export function nextLocalMidnight(now: number): number {
  const d = new Date(now)
  return new Date(d.getFullYear(), d.getMonth(), d.getDate() + 1).getTime()
}

/** tooltip 的完整時間，本地時區 `YYYY-MM-DD HH:mm:ss`；讀不懂就原樣顯示。 */
export function msgTimeTitle(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`
}
