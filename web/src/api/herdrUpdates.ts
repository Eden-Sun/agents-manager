/**
 * `GET /api/herdr/updates`：Herdr 自己的版本追蹤（daemon `herdr_updates.rs`）。
 *
 * 面板的存在理由就是「不要以為自己是最新的」，所以正規化的規則跟別處不同：**任何看不懂的值都收成
 * `unknown`，絕不收成 `latest`**。舊 daemon（沒有這個端點）回 404，那也是未知，不是最新。
 */
import { rawTransport } from './index'

/** `unknown` = 任一邊不知道。`ahead` = 本機比官方 stable 新（跑 prerelease 或自己 build）。 */
export type Standing = 'unknown' | 'latest' | 'behind' | 'ahead'

const STANDINGS: Standing[] = ['unknown', 'latest', 'behind', 'ahead']

export interface VersionSide {
  version: string | null
  /** 這個版本號是什麼時候讀到的；`null` = 從來沒讀到過。 */
  at: string | null
  error: string | null
  standing: Standing
  /** 只有 running server 有（herdr `ping` 的 protocol）。 */
  protocol: number | null
}

export interface HerdrHost {
  host: string
  connected: boolean
  checkedAt: string | null
  server: VersionSide
  disk: VersionSide
  /** 磁碟上比跑著的 server 新 = 新版已經裝好，換 server 才會生效。 */
  restartPending: boolean
}

export interface NoteSection {
  version: string
  /** `null` = 官方那一筆沒附 notes。不要拿別版的內容補。 */
  notes: string | null
}

export interface HerdrNotes {
  from: string | null
  to: string | null
  /** `false` = 起點那一版不在官方清單裡，中間可能還有沒列出來的版本。 */
  complete: boolean
  gap: string | null
  missingNotes: string[]
  sections: NoteSection[]
}

export interface HerdrLatest {
  version: string | null
  protocol: number | null
  endpointGeneration: number | null
  fetchedAt: string | null
  checkedAt: string | null
  /** 太久沒抓成功：值照舊顯示，但要標示成舊資料。 */
  stale: boolean
  error: string | null
  sourceUrl: string
  releasesUrl: string
}

export interface HerdrUpdates {
  latest: HerdrLatest
  hosts: HerdrHost[]
  behindHosts: string[]
  /** 連跑著的版本都問不到的主機：顯示為未知，不併進「已是最新」。 */
  unknownHosts: string[]
  notes: HerdrNotes
  notice: { version: string; notifiedAt: string | null; seenAt: string | null } | null
  unread: boolean
  /** daemon 承諾這支只讀：UI 最多只給「重新檢查」與官方連結。 */
  readOnly: boolean
  /** 整包拿不到時的原因（端點不存在、沒權限、daemon 掛了）。 */
  error: string | null
}

export const SOURCE_URL = 'https://herdr.dev/latest.json'
export const RELEASES_URL = 'https://github.com/herdrdev/herdr/releases'

function isRec(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null
}

function str(v: unknown): string | null {
  return typeof v === 'string' && v.trim() !== '' ? v : null
}

function num(v: unknown): number | null {
  return typeof v === 'number' && Number.isFinite(v) ? v : null
}

function standing(v: unknown): Standing {
  return STANDINGS.includes(v as Standing) && v !== undefined ? (v as Standing) : 'unknown'
}

function side(raw: unknown): VersionSide {
  const r = isRec(raw) ? raw : {}
  const version = str(r.version)
  return {
    version,
    at: str(r.at),
    error: str(r.error),
    // 沒有版本就一定是未知，不管 daemon 說了什麼。
    standing: version ? standing(r.standing) : 'unknown',
    protocol: num(r.protocol),
  }
}

function host(raw: unknown): HerdrHost | null {
  const r = isRec(raw) ? raw : {}
  const name = str(r.host)
  if (!name) return null
  return {
    host: name,
    connected: r.connected === true,
    checkedAt: str(r.checked_at),
    server: side(r.server),
    disk: side(r.disk),
    restartPending: r.restart_pending === true,
  }
}

function notes(raw: unknown): HerdrNotes {
  const r = isRec(raw) ? raw : {}
  const sections = Array.isArray(r.sections)
    ? r.sections
        .filter(isRec)
        .map((s) => ({ version: String(s.version ?? ''), notes: str(s.notes) }))
        .filter((s) => s.version)
    : []
  return {
    from: str(r.from),
    to: str(r.to),
    complete: r.complete === true,
    gap: str(r.gap),
    missingNotes: Array.isArray(r.missing_notes) ? r.missing_notes.filter((v): v is string => typeof v === 'string') : [],
    sections,
  }
}

const UNKNOWN_LATEST: HerdrLatest = {
  version: null,
  protocol: null,
  endpointGeneration: null,
  fetchedAt: null,
  checkedAt: null,
  stale: true,
  error: null,
  sourceUrl: SOURCE_URL,
  releasesUrl: RELEASES_URL,
}

export function emptyUpdates(error: string | null): HerdrUpdates {
  return {
    latest: { ...UNKNOWN_LATEST, error },
    hosts: [],
    behindHosts: [],
    unknownHosts: [],
    notes: { from: null, to: null, complete: false, gap: null, missingNotes: [], sections: [] },
    notice: null,
    unread: false,
    readOnly: true,
    error,
  }
}

export function toUpdates(raw: unknown): HerdrUpdates {
  const r = isRec(raw) ? raw : {}
  const l = isRec(r.latest) ? r.latest : {}
  const hosts = Array.isArray(r.hosts) ? r.hosts.map(host).filter((h): h is HerdrHost => h !== null) : []
  const noticeRaw = isRec(r.notice) ? r.notice : null
  const noticeVersion = noticeRaw ? str(noticeRaw.version) : null
  return {
    latest: {
      version: str(l.version),
      protocol: num(l.protocol),
      endpointGeneration: num(l.endpoint_generation),
      fetchedAt: str(l.fetched_at),
      checkedAt: str(l.checked_at),
      // 沒給就當舊資料：寧可多提醒一次「這是上次查到的」，也不要把陳年快取講成剛確認過。
      stale: l.stale !== false,
      error: str(l.error),
      sourceUrl: str(l.source_url) ?? SOURCE_URL,
      releasesUrl: str(l.releases_url) ?? RELEASES_URL,
    },
    hosts,
    behindHosts: hosts.filter((h) => h.server.standing === 'behind' || h.disk.standing === 'behind').map((h) => h.host),
    unknownHosts: hosts.filter((h) => h.server.version === null).map((h) => h.host),
    notes: notes(r.notes),
    notice: noticeVersion
      ? { version: noticeVersion, notifiedAt: str(noticeRaw?.notified_at), seenAt: str(noticeRaw?.seen_at) }
      : null,
    unread: r.unread === true,
    readOnly: r.read_only !== false,
    error: null,
  }
}

/** mock 模式（`VITE_MOCK=1`）給一組實測形狀的資料：本機 0.8.2、官方 0.9.0、一台離線遠端。 */
function mockPayload(): unknown {
  return {
    latest: {
      version: '0.9.0',
      protocol: 22,
      endpoint_generation: 1,
      fetched_at: new Date(Date.now() - 42 * 60 * 1000).toISOString(),
      checked_at: new Date(Date.now() - 42 * 60 * 1000).toISOString(),
      stale: false,
      error: null,
      source_url: SOURCE_URL,
      releases_url: RELEASES_URL,
    },
    hosts: [
      {
        host: 'local',
        connected: true,
        checked_at: new Date().toISOString(),
        server: { version: '0.8.2', protocol: 20, at: new Date().toISOString(), error: null, standing: 'behind' },
        disk: { version: '0.8.2', at: new Date().toISOString(), error: null, standing: 'behind' },
        restart_pending: false,
      },
      {
        host: 'box',
        connected: false,
        checked_at: new Date().toISOString(),
        server: {
          version: '0.8.0',
          protocol: 19,
          at: new Date(Date.now() - 26 * 3600 * 1000).toISOString(),
          error: '主機未連線，跑著的 herdr server 版本是上次讀到的',
          standing: 'behind',
        },
        disk: { version: null, at: null, error: 'ssh 失敗：Connection refused', standing: 'unknown' },
        restart_pending: false,
      },
    ],
    behind_hosts: ['box', 'local'],
    unknown_hosts: [],
    notes: {
      from: '0.8.0',
      to: '0.9.0',
      complete: true,
      gap: null,
      missing_notes: [],
      sections: [
        { version: '0.9.0', notes: '### Added\n- Manage Local and saved SSH machines from one Herdr window. (#3670)' },
        { version: '0.8.2', notes: '### Fixed\n- Pane reads no longer return empty text. (#3444)' },
      ],
    },
    notice: { version: '0.9.0', notified_at: new Date(Date.now() - 40 * 60 * 1000).toISOString(), seen_at: null },
    unread: true,
    read_only: true,
  }
}

async function call(method: 'GET' | 'POST', path: string, body?: unknown): Promise<HerdrUpdates> {
  if (rawTransport.mock) return toUpdates(mockPayload())
  try {
    return toUpdates(await rawTransport.request(method, path, body))
  } catch (e) {
    return emptyUpdates(e instanceof Error ? e.message : String(e))
  }
}

export function fetchHerdrUpdates(): Promise<HerdrUpdates> {
  return call('GET', '/herdr/updates')
}

/** 手動重查。節流與併發合流都在 daemon 那邊，這裡不另外擋。 */
export function refreshHerdrUpdates(): Promise<HerdrUpdates> {
  return call('POST', '/herdr/updates/refresh')
}

/** 按掉這一版的未讀提示。 */
export function markHerdrUpdateSeen(version: string): Promise<HerdrUpdates> {
  return call('POST', '/herdr/updates/seen', { version })
}
