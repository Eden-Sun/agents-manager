import { t } from './i18n'

export function isRemoteHttpImage(src: string): boolean {
  return /^https?:/i.test(src.trim())
}

const SUSPICIOUS_TAIL = 64

export interface RemoteImageInfo {
  host: string
  suspicious: boolean
  internal: boolean
}

function isInternalV4(a: number, b: number): boolean {
  return a === 0 || a === 10 || a === 127 || (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168)
}

function isInternalHost(hostname: string): boolean {
  const h = hostname.toLowerCase()
  if (h === 'localhost' || h.endsWith('.localhost') || h.endsWith('.local')) return true
  if (h.startsWith('[')) {
    const v6 = h.slice(1, -1)
    const mapped = /^::ffff:([0-9a-f]{1,4}):([0-9a-f]{1,4})$/.exec(v6)
    if (mapped) {
      const hi = parseInt(mapped[1], 16)
      return isInternalV4(hi >> 8, hi & 0xff)
    }
    return v6 === '::1' || v6 === '::' || /^f[cd][0-9a-f]{2}:/.test(v6) || /^fe[89ab][0-9a-f]:/.test(v6)
  }
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(h)
  if (!m) return false
  return isInternalV4(Number(m[1]), Number(m[2]))
}

export function remoteImageInfo(src: string): RemoteImageInfo | null {
  if (!isRemoteHttpImage(src)) return null
  try {
    const u = new URL(src.trim())
    return {
      host: u.hostname,
      suspicious: u.username !== '' || u.password !== '' || u.search.length + u.hash.length > SUSPICIOUS_TAIL,
      internal: isInternalHost(u.hostname),
    }
  } catch {
    return null
  }
}

export const REMOTE_IMAGE_MAX_BYTES = 10 * 1024 * 1024

const SAFE_IMAGE_TYPES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp', 'image/avif', 'image/bmp']
export function safeImageType(contentType: string): string | null {
  const t = contentType.split(';')[0].trim().toLowerCase()
  return SAFE_IMAGE_TYPES.includes(t) ? t : null
}

async function readCapped(res: Response, type: string): Promise<Blob> {
  const declared = Number(res.headers.get('content-length'))
  if (Number.isFinite(declared) && declared > REMOTE_IMAGE_MAX_BYTES) throw new Error(t('圖片太大'))
  if (!res.body) {
    const b = await res.blob()
    if (b.size > REMOTE_IMAGE_MAX_BYTES) throw new Error(t('圖片太大'))
    return new Blob([b], { type })
  }
  const reader = res.body.getReader()
  const chunks: Uint8Array[] = []
  let total = 0
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    total += value.byteLength
    if (total > REMOTE_IMAGE_MAX_BYTES) {
      await reader.cancel().catch(() => {})
      throw new Error(t('圖片太大'))
    }
    chunks.push(value)
  }
  return new Blob(chunks as BlobPart[], { type })
}

export async function fetchRemoteImageBlob(src: string, fetchImpl: typeof fetch = fetch): Promise<Blob> {
  const res = await fetchImpl(src, { mode: 'cors', credentials: 'omit', referrerPolicy: 'no-referrer', redirect: 'error' })
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  const raw = res.headers.get('content-type') ?? ''
  const type = safeImageType(raw)
  if (!type) throw new Error(/^image\//i.test(raw) ? t('不支援的圖片格式（SVG 等可以帶腳本的不收）') : t('不是圖片'))
  return readCapped(res, type)
}
