/**
 * `GET /api/herdr/updates`：Herdr 自己的版本追蹤（daemon `herdr_updates.rs`）。
 *
 * 面板的存在理由就是「不要以為自己是最新的」，所以正規化的規則跟別處不同：**任何看不懂的值都收成
 * `unknown`，絕不收成 `latest`**。這裡只負責「把 payload 讀成型別」與「打哪支端點」；失敗要怎麼跟
 * 上一份好資料合併，是 `hooks/useHerdrUpdates.ts` 的事——只有它知道上一份長什麼樣。
 */
import { rawTransport } from './index'
import { ApiError } from './types'

/** `unknown` = 現在不知道（沒讀到、讀數過期、官方版本不明）。`ahead` = 比官方 stable 新。 */
export type Standing = 'unknown' | 'latest' | 'behind' | 'ahead'

const STANDINGS: Standing[] = ['unknown', 'latest', 'behind', 'ahead']

export interface VersionSide {
  /** 最後一次讀到的版本；不保證是現在的實況（看 `fresh`）。 */
  version: string | null
  /** 這個版本號是什麼時候讀到的；`null` = 從來沒讀到過。 */
  at: string | null
  error: string | null
  /** 這筆讀數現在還算不算數。false = 畫面上要說「上次讀到」。 */
  fresh: boolean
  /** **現在**的判斷。不新鮮或官方版本不明時一律 `unknown`。 */
  standing: Standing
  /** 上次讀到的那個版本對現在已知的最新版——只能當歷史說明，不能當現況。 */
  cachedStanding: Standing
  /** 只有 running server 有（herdr `ping` 的 protocol）。 */
  protocol: number | null
}

export interface HerdrHost {
  host: string
  connected: boolean
  checkedAt: string | null
  /** 兩邊都是剛確認過的。 */
  fresh: boolean
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
  /** `false` = 起點不在官方清單裡、或中間有版本沒附內容，這份清單不是完整的跨版差距。 */
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
  /** 太久沒抓成功：值照舊顯示，但不能當成現況，也不能拿來發綠燈。 */
  stale: boolean
  error: string | null
  sourceUrl: string
  releasesUrl: string
}

export interface HerdrUpdates {
  latest: HerdrLatest
  hosts: HerdrHost[]
  behindHosts: string[]
  /** 現在不知道狀態的主機（含讀數過期）。不得併進「都最新」。 */
  unknownHosts: string[]
  /** 讀數已經不算數的主機。 */
  staleHosts: string[]
  notes: HerdrNotes
  notice: { version: string; notifiedAt: string | null; seenAt: string | null } | null
  unread: boolean
  /** daemon 承諾這支只讀：UI 最多只給「重新檢查」與官方連結。 */
  readOnly: boolean
  /** 這份資料上次跟 daemon 對上的時間（毫秒），由 store 用它自己的時鐘蓋上；`null` = 還沒對上過。 */
  syncedAt: number | null
  /** 最近一次跟 daemon 說話失敗的原因；有值時上面的資料是上次拿到的。 */
  error: string | null
  /** 只有 404 才算「這個 daemon 沒有這個功能」。其他錯誤是暫時連不上。 */
  unsupported: boolean
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
  return STANDINGS.includes(v as Standing) ? (v as Standing) : 'unknown'
}

function side(raw: unknown): VersionSide {
  const r = isRec(raw) ? raw : {}
  const version = str(r.version)
  // 沒有版本就一定是未知，不管 daemon 說了什麼；舊 daemon 沒給 `fresh` 也一律當不新鮮。
  const fresh = version !== null && r.fresh === true
  return {
    version,
    at: str(r.at),
    error: str(r.error),
    fresh,
    standing: fresh ? standing(r.standing) : 'unknown',
    cachedStanding: version ? standing(r.cached_standing ?? r.standing) : 'unknown',
    protocol: num(r.protocol),
  }
}

function host(raw: unknown): HerdrHost | null {
  const r = isRec(raw) ? raw : {}
  const name = str(r.host)
  if (!name) return null
  const server = side(r.server)
  const disk = side(r.disk)
  return {
    host: name,
    connected: r.connected === true,
    checkedAt: str(r.checked_at),
    fresh: server.fresh && disk.fresh,
    server,
    disk,
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
  const missingNotes = Array.isArray(r.missing_notes) ? r.missing_notes.filter((v): v is string => typeof v === 'string') : []
  return {
    from: str(r.from),
    to: str(r.to),
    // 少一段內容就不是完整的跨版差距——舊 daemon 只看 `from` 在不在，這裡再擋一次。
    complete: r.complete === true && missingNotes.length === 0 && sections.every((s) => s.notes !== null),
    gap: str(r.gap),
    missingNotes,
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

/** 什麼都還不知道。只在「從來沒成功拿到過」時使用——有舊資料時要用 `degraded` 保留它。 */
export function emptyUpdates(error: string | null, unsupported = false): HerdrUpdates {
  return {
    latest: { ...UNKNOWN_LATEST, error },
    hosts: [],
    behindHosts: [],
    unknownHosts: [],
    staleHosts: [],
    notes: { from: null, to: null, complete: false, gap: null, missingNotes: [], sections: [] },
    notice: null,
    unread: false,
    readOnly: true,
    syncedAt: null,
    error,
    unsupported,
  }
}

/**
 * API 暫時斷線：**留著上一份好資料**，只標成過期並寫上原因。
 *
 * 一次網路抖動把整張卡清成「什麼都不知道」，跟 daemon 那邊「抓失敗不吞舊快取」的規則自相矛盾，
 * 而且會讓已經亮出來的「有新版」憑空消失。
 */
export function degraded(prev: HerdrUpdates, error: string): HerdrUpdates {
  return { ...prev, latest: { ...prev.latest, stale: true, error }, error, unsupported: false }
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
    // 三份清單都自己從每台的狀態算，不信 daemon 給的總數。
    behindHosts: hosts.filter((h) => h.server.standing === 'behind' || h.disk.standing === 'behind').map((h) => h.host),
    unknownHosts: hosts.filter((h) => h.server.standing === 'unknown' || h.disk.standing === 'unknown').map((h) => h.host),
    staleHosts: hosts.filter((h) => !h.fresh).map((h) => h.host),
    notes: notes(r.notes),
    notice: noticeVersion
      ? { version: noticeVersion, notifiedAt: str(noticeRaw?.notified_at), seenAt: str(noticeRaw?.seen_at) }
      : null,
    unread: r.unread === true,
    readOnly: r.read_only !== false,
    // 時間戳交給 store：它才有那支（測試裡是假的）時鐘。
    syncedAt: null,
    error: null,
    unsupported: false,
  }
}

/** mock 模式（`VITE_MOCK=1`）給一組實測形狀的資料：本機 0.8.2、官方 0.9.0、一台離線遠端。 */
function mockPayload(): unknown {
  const now = new Date().toISOString()
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
        checked_at: now,
        fresh: true,
        server: { version: '0.8.2', protocol: 20, at: now, error: null, fresh: true, standing: 'behind', cached_standing: 'behind' },
        disk: { version: '0.8.2', at: now, error: null, fresh: true, standing: 'behind', cached_standing: 'behind' },
        restart_pending: false,
      },
      {
        host: 'box',
        connected: false,
        checked_at: now,
        fresh: false,
        server: {
          version: '0.8.0',
          protocol: 19,
          at: new Date(Date.now() - 26 * 3600 * 1000).toISOString(),
          error: '主機未連線，跑著的 herdr server 版本是上次讀到的',
          fresh: false,
          standing: 'unknown',
          cached_standing: 'behind',
        },
        disk: { version: null, at: null, error: 'ssh 失敗：Connection refused', fresh: false, standing: 'unknown', cached_standing: 'unknown' },
        restart_pending: false,
      },
    ],
    behind_hosts: ['local'],
    unknown_hosts: ['box'],
    stale_hosts: ['box'],
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

/** 連不上 / 沒這支端點。`status` 只有 404 才代表「這個 daemon 不支援」。 */
export class HerdrUpdatesError extends Error {
  readonly status: number | null
  constructor(message: string, status: number | null) {
    super(message)
    this.name = 'HerdrUpdatesError'
    this.status = status
  }
}

async function call(method: 'GET' | 'POST', path: string, body?: unknown): Promise<HerdrUpdates> {
  if (rawTransport.mock) return toUpdates(mockPayload())
  try {
    return toUpdates(await rawTransport.request(method, path, body))
  } catch (e) {
    const status = e instanceof ApiError ? e.status : null
    throw new HerdrUpdatesError(e instanceof Error ? e.message : String(e), status)
  }
}

export function fetchHerdrUpdates(): Promise<HerdrUpdates> {
  return call('GET', '/herdr/updates')
}

/** 手動重查（daemon 會真的打官方站）。節流與併發合流都在 daemon 那邊，這裡不另外擋。 */
export function refreshHerdrUpdates(): Promise<HerdrUpdates> {
  return call('POST', '/herdr/updates/refresh')
}

/** 按掉這一版的未讀提示。 */
export function markHerdrUpdateSeen(version: string): Promise<HerdrUpdates> {
  return call('POST', '/herdr/updates/seen', { version })
}
