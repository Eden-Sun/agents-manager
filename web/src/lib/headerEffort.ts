import { effortLabel } from '../api/types'

/** 標題列模型旁的強度縮寫（2026-11-01 使用者：模型選單維持全名，只有標題列縮）：Medium=M、Max=X；只換顯示，原值不變。 */
export function headerEffortLabel(effort: string): string {
  const short: Record<string, string> = { low: 'L', medium: 'M', high: 'H', xhigh: 'XH', max: 'X', ultra: 'U' }
  return short[effort.toLowerCase()] ?? effortLabel(effort)
}

const EFFORT_WORDS = /^(?:none|minimal|low|medium|high|xhigh|max|ultra|default)$/i

/**
 * Codex 標題列要顯示的強度，依序取第一個有的：終端 footer 的實際值 → daemon 的 status_json → run 的 status_line（遠端、
 * 沒掛終端時就只剩這一條）→ run 實際啟動值（`runtime_effort`，沒有才是 bot 設定）→ 模型 slug。
 * 不認識的文字（不是強度詞）一律略過，不當成強度。
 */
export function codexHeaderEffort(c: {
  terminalModelLine: string | null
  statusEffort: string | null
  statusLineModelLine: string | null
  runEffort: string | null
  slugEffort: string | null
  parse: (line: string) => { effort: string | null }
}): string | null {
  const pick = (v: string | null | undefined) => (v && EFFORT_WORDS.test(v) ? v : null)
  return (
    pick(c.terminalModelLine ? c.parse(c.terminalModelLine).effort : null) ??
    pick(c.statusEffort) ??
    pick(c.statusLineModelLine ? c.parse(c.statusLineModelLine).effort : null) ??
    pick(c.runEffort) ??
    pick(c.slugEffort)
  )
}
