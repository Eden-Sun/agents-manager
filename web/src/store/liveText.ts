
const CHROME_ONLY = /^[\s>\u2500-\u257f\u2580-\u259f·•◦…‥.]*$/

const NOISE: RegExp[] = [
  /^\s*(?:※\s*)?Tips?\s*[:：]/i,
  /Auto-?update\s+(?:failed|available|installed)/i,
  /\brun\s+claude\s+doctor\b/i,
  /^\s*[(（]?\s*(?:ctrl|cmd|⌘|shift|alt|option)\s*\+/i,
  /^\s*[(（]?\s*(?:esc|escape)\s+(?:\w+\s+)?(?:to|for)\s+\w+\s*[)）]?\s*(?:[·•|]|$)/i,
  /^\s*[(（]?\s*\?\s+for\s+shortcuts\s*[)）]?\s*$/i,
  /^\s*[(（]?\s*\?\s+for\s+shortcuts\s{2,}\S/i,
  /^\s*⚠ \d+ warnings? · f2 to view\s*$/,
  /OpenAI Codex \(v/i,
  /Ask Codex to do anything/i,
  /autocompletes slash commands/i,
  /usage limit reset available/i,
  /\/model to change/i,
  /^\s*directory:\s/i,
  /^permissions:\s*YOLO/i,
  /Context \d+%\s*used/i,
  /^\s*[⎿⏺]/,
  /^\s*[✻✽✢✳✶*+·⠀-⣿]\s+\S+…/,
  /^\s*\S+…\s*[(（]\d+s\b/,
]

const SUMMARY_VERB =
  /^(?:ran|read|wrote|edited|added|removed|deleted|created|updated|searched|listed|fetched|explored|analy[sz]ed|committed|pushed|pulled|running|reading|writing|editing|searching|fetching|thinking|thought|called|did)\b/i

const SHELL_ECHO = /^\s*\$\s+[a-z][\w./-]*(?:[\s;|]|$)/

const TOOL_ELAPSED = /·\s*\d+(?:\.\d+)?\s*[sm]\b/
const SHELL_HINT = /[|;]|\s--?[a-z]/

const SPINNER_DONE = /^\s*[*✻✽✢✳✶·]?\s*(?:[A-Za-z]+(?:ed|ing)|Thought)\s+for\s+\d+(?:\.\d+)?\s*[smh](?:\s+\d+\s*[sm])?(?:\s*[·(（].*)?\s*$/

const TOOL_TAIL = /\([\d.]+\s*[smh]\s*·\s*\d+\s+lines?\)\s*$/

const TRAILING_STATE = /\s*·\s*(?:summari[sz]ed|compacted|truncated|cancell?ed|interrupted)\s*$/i

const HAS_WORD = /\p{L}/u

function isCliSummary(line: string): boolean {
  if (/[.。！!？?：:;；]$/.test(line)) return false
  if (/[\u3400-\u9fff\u3040-\u30ff]/.test(line)) return false
  if (!/\d/.test(line) || line.length > 120) return false
  return line.split(/,\s*/).every((clause) => SUMMARY_VERB.test(clause))
}

function isModeRow(t: string): boolean {
  if (t.startsWith('⏵⏵')) return true
  const low = t.toLowerCase()
  const rest = low.startsWith('⏸') ? low.slice('⏸'.length) : low
  const seg = (rest.split(' · ')[0] ?? '').trim()
  return seg.endsWith('(shift+tab to cycle)') || ['manual mode on', 'plan mode on', 'accept edits on', 'bypass permissions on'].includes(seg)
}

function isCodexStatusRow(t: string): boolean {
  const i = t.indexOf('·')
  if (i < 0) return false
  const model = (t.slice(0, i).trim().split(/\s+/)[0] ?? '')
  if (!model.includes('-') || !/^[a-z0-9]/i.test(model)) return false
  const rest = t.slice(i + 1).trim()
  return rest.startsWith('/') || rest.startsWith('~') || rest.includes('Context') || rest.includes('% left')
}

function isNoise(line: string): boolean {
  const t = line.trim()
  if (!t) return false
  if (CHROME_ONLY.test(t)) return true
  if (isModeRow(t) || isCodexStatusRow(t)) return true
  if (SHELL_ECHO.test(t)) return true
  if (/^(?:running|ran)\b[^.]*\bshell command/i.test(t)) return true
  if (SPINNER_DONE.test(t)) return true
  if (TOOL_TAIL.test(t)) return true
  if (TOOL_ELAPSED.test(t) && SHELL_HINT.test(t)) return true
  if (/^\s*❯\s/.test(t)) return true
  if (isCliSummary(t)) return true
  return NOISE.some((re) => re.test(t))
}

const CACHE = new Map<string, string | null>()
const CACHE_MAX = 16

function scrub(raw: string): string | null {
  const kept: string[] = []
  for (const raw_line of raw.split('\n')) {
    const line = raw_line.replace(TRAILING_STATE, '')
    if (isNoise(line)) continue
    if (!line.trim() && (kept.length === 0 || !kept[kept.length - 1].trim())) continue
    kept.push(line)
  }
  const out = kept.join('\n').trim()
  return out && HAS_WORD.test(out) ? out : null
}

export function cleanLiveText(raw: string | null | undefined): string | null {
  if (!raw || !raw.trim()) return null
  const hit = CACHE.get(raw)
  if (hit !== undefined) return hit
  const out = scrub(raw)
  if (CACHE.size >= CACHE_MAX) CACHE.delete(CACHE.keys().next().value as string)
  CACHE.set(raw, out)
  return out
}

const ACTIVITY_HINT = /\s*[(（][^()（）]*\b(?:ctrl|cmd|⌘|shift|alt|esc|escape|tab)\b[^()（）]*[)）]/gi
const ACTIVITY_TAIL = /\s*[·•|]?\s*\b(?:esc|escape)\s+(?:to|for)\b.*$/i

export function cleanLiveActivity(raw: string | null | undefined): string | null {
  if (!raw) return null
  const out = raw.replace(ACTIVITY_HINT, '').replace(ACTIVITY_TAIL, '').trim()
  return out && HAS_WORD.test(out) ? out : null
}
