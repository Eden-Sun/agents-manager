// 排程 prompt 的變數（與 daemon/src/schedules/template.rs 的 VARS 同一份清單）。
// 送出時由 daemon 展開；這裡只負責編輯器的插入、未知變數提示與「預覽展開結果」。

export const PROMPT_VARS = ['time', 'date', 'weekday', 'name', 'slot'] as const

const TOKEN = /\{\{([^{}]*)\}\}/g

export interface VarContext {
  time: string
  date: string
  weekday: string
  name: string
  slot: string
}

/** 把 `{{…}}` 換成值；不認得的原樣保留（與 daemon 相同）。 */
export function expandPrompt(prompt: string, ctx: VarContext): string {
  return prompt.replace(TOKEN, (whole, key: string) => {
    return (PROMPT_VARS as readonly string[]).includes(key) ? ctx[key as keyof VarContext] : whole
  })
}

/** 內容裡出現、但不認得的變數名（去重，保留出現順序）。 */
export function unknownPromptVars(prompt: string): string[] {
  const out: string[] = []
  for (const m of prompt.matchAll(TOKEN)) {
    const key = m[1]
    if (!(PROMPT_VARS as readonly string[]).includes(key) && !out.includes(key)) out.push(key)
  }
  return out
}

const WEEKDAY_ZH = ['日', '一', '二', '三', '四', '五', '六']

/** `when` 在 `tz` 時區的 HH:MM、YYYY-MM-DD、星期（預覽用）。 */
export function contextAt(tz: string, when: Date, name: string, slot: string): VarContext {
  const parts = new Intl.DateTimeFormat('en-CA', { timeZone: tz, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hourCycle: 'h23', weekday: 'short' }).formatToParts(when)
  const get = (type: string) => parts.find((p) => p.type === type)?.value ?? ''
  const weekdayIdx = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'].indexOf(get('weekday'))
  return {
    time: `${get('hour')}:${get('minute')}`,
    date: `${get('year')}-${get('month')}-${get('day')}`,
    weekday: weekdayIdx >= 0 ? WEEKDAY_ZH[weekdayIdx] : '',
    name,
    slot,
  }
}
