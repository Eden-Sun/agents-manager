import { effortLabel } from '../api/types'

export function headerEffortLabel(effort: string): string {
  const short: Record<string, string> = { low: 'L', medium: 'M', high: 'H', xhigh: 'XH', max: 'X', ultra: 'U' }
  return short[effort.toLowerCase()] ?? effortLabel(effort)
}

const EFFORT_WORDS = /^(?:none|minimal|low|medium|high|xhigh|max|ultra|default)$/i

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
