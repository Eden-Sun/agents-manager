/**
 * `GET /api/changelog?kind=&host=&from=`：「有更新 · 重啟套用」按下去先看新版改了什麼
 * （2026-09-10 使用者需求）。獨立成一檔，`index.ts` 只借 transport。
 *
 * daemon 抓不到時仍回 200（`found:false` + `error`）；這裡再把傳輸層的錯（舊 daemon 404、
 * mock 模式、斷線）也收成同一個形狀，UI 一律有東西可寫，不會靜默略過。
 */
import { rawTransport } from './index'

export interface ChangelogSection {
  version: string
  body: string
}

export interface ChangelogReply {
  kind: string
  host: string
  installedVersion: string | null
  fromVersion: string | null
  found: boolean
  sections: ChangelogSection[]
  sourceUrl: string
  error: string | null
}

const CLAUDE_SOURCE = 'https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md'
const CODEX_SOURCE = 'https://github.com/openai/codex/releases'

/** daemon 回不了東西時的退路連結，要跟 kind 對得上（codex 沒有 CHANGELOG.md，只有 releases）。 */
function sourceFor(kind: string): string {
  return kind === 'codex' ? CODEX_SOURCE : CLAUDE_SOURCE
}

function isRec(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null
}

function toReply(raw: unknown, kind: string, host: string): ChangelogReply {
  const r = isRec(raw) ? raw : {}
  const sections = Array.isArray(r.sections)
    ? r.sections
        .filter(isRec)
        .map((s) => ({ version: String(s.version ?? ''), body: String(s.body ?? '') }))
        .filter((s) => s.version)
    : []
  return {
    kind: typeof r.kind === 'string' ? r.kind : kind,
    host: typeof r.host === 'string' ? r.host : host,
    installedVersion: typeof r.installed_version === 'string' ? r.installed_version : null,
    fromVersion: typeof r.from_version === 'string' ? r.from_version : null,
    found: r.found === true && sections.length > 0,
    sections,
    sourceUrl: typeof r.source_url === 'string' ? r.source_url : sourceFor(kind),
    error: typeof r.error === 'string' ? r.error : null,
  }
}

export async function fetchChangelog(
  kind: string,
  host: string,
  from: string | null,
  to: string | null = null,
): Promise<ChangelogReply> {
  const q = new URLSearchParams({ kind, host })
  if (from) q.set('from', from)
  // codex：新版還沒進磁碟，目標版本由畫面上那句 `0.153.4 -> 0.154.0` 帶進來。
  if (to) q.set('to', to)
  if (rawTransport.mock) {
    return { kind, host, installedVersion: null, fromVersion: from, found: false, sections: [], sourceUrl: sourceFor(kind), error: 'mock 模式沒有 changelog' }
  }
  try {
    return toReply(await rawTransport.request('GET', `/changelog?${q.toString()}`), kind, host)
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    return { kind, host, installedVersion: null, fromVersion: from, found: false, sections: [], sourceUrl: sourceFor(kind), error: msg }
  }
}
