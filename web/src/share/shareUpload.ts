import type { ShareClient } from './shareApi'
import { SHARE_FILE_MAX, ShareHttpError } from './shareModel'
import { t } from './shareI18n'

export const PHOTO_MAX_EDGE = 2048
const JPEG_QUALITY = 0.85
const RETRY_MAX = 12
const RETRY_WAIT_MAX_S = 30
const RETRY_503_MAX = 3
const RETRY_TOTAL_MAX = RETRY_MAX + RETRY_503_MAX

export class PhotoUnreadableError extends Error {
  constructor() {
    super('photo_unreadable')
  }
}

export class UploadCancelledError extends Error {
  constructor() {
    super('upload_cancelled')
  }
}

const throwIfCancelled = (signal?: AbortSignal) => {
  if (signal?.aborted) throw new UploadCancelledError()
}

function untilCancelled<T>(p: Promise<T>, signal?: AbortSignal): Promise<T> {
  if (!signal) return p
  throwIfCancelled(signal)
  return new Promise<T>((resolve, reject) => {
    const onAbort = () => reject(new UploadCancelledError())
    signal.addEventListener('abort', onAbort, { once: true })
    p.then(
      (v) => {
        signal.removeEventListener('abort', onAbort)
        resolve(v)
      },
      (e) => {
        signal.removeEventListener('abort', onAbort)
        reject(e)
      },
    )
  })
}

const isHeic = (f: File) => /^image\/hei[cf]/i.test(f.type) || /\.hei[cf]s?$/i.test(f.name)
const isJpeg = (f: File) => /^image\/jpe?g$/i.test(f.type) || /\.jpe?g$/i.test(f.name)

function jpegName(name: string): string {
  const base = name.replace(/\.[^./\\]*$/, '')
  return `${base || 'photo'}.jpg`
}

export interface ImageCodec {
  decode(f: File): Promise<{ width: number; height: number; source: CanvasImageSource; close(): void } | null>
  encodeJpeg(source: CanvasImageSource, width: number, height: number, quality: number): Promise<Blob | null>
}

const browserCodec: ImageCodec = {
  async decode(f) {
    if (typeof createImageBitmap !== 'function') return null
    try {
      const bmp = await createImageBitmap(f)
      return { width: bmp.width, height: bmp.height, source: bmp, close: () => bmp.close() }
    } catch {
      return null
    }
  },
  async encodeJpeg(source, width, height, quality) {
    const canvas = document.createElement('canvas')
    canvas.width = width
    canvas.height = height
    const ctx = canvas.getContext('2d')
    if (!ctx) return null
    ctx.fillStyle = '#fff'
    ctx.fillRect(0, 0, width, height)
    ctx.drawImage(source, 0, 0, width, height)
    return new Promise((resolve) => canvas.toBlob((b) => resolve(b), 'image/jpeg', quality))
  },
}

export async function preparePhoto(f: File, codec: ImageCodec = browserCodec): Promise<File> {
  const heic = isHeic(f)
  if (!heic && !isJpeg(f)) return f
  const img = await codec.decode(f)
  if (!img) {
    if (heic) throw new PhotoUnreadableError()
    return f
  }
  try {
    const long = Math.max(img.width, img.height)
    if (!heic && long <= PHOTO_MAX_EDGE) return f
    const scale = Math.min(1, PHOTO_MAX_EDGE / long)
    const w = Math.max(1, Math.round(img.width * scale))
    const h = Math.max(1, Math.round(img.height * scale))
    const blob = await codec.encodeJpeg(img.source, w, h, JPEG_QUALITY)
    if (!blob) {
      if (heic) throw new PhotoUnreadableError()
      return f
    }
    if (!heic && blob.size >= f.size) return f
    return new File([blob], heic ? jpegName(f.name) : f.name.replace(/\.jpeg$/i, '.jpg'), { type: 'image/jpeg', lastModified: f.lastModified })
  } finally {
    img.close()
  }
}

export interface UploadOptions {
  sleep?: (ms: number) => Promise<void>
  codec?: ImageCodec
  signal?: AbortSignal
}

const defaultSleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms))

export async function uploadPatiently(client: ShareClient, f: File, opts: UploadOptions = {}): Promise<{ id: string; name: string }> {
  const sleep = opts.sleep ?? defaultSleep
  const { signal } = opts
  throwIfCancelled(signal)
  const file = await untilCancelled(preparePhoto(f, opts.codec), signal)
  if (file.size > SHARE_FILE_MAX) throw new ShareHttpError(413)
  let rateLimited = 0
  let busy = 0
  let retries = 0
  for (;;) {
    throwIfCancelled(signal)
    try {
      return await untilCancelled(client.upload(file, signal), signal)
    } catch (e) {
      if (!(e instanceof ShareHttpError) || retries >= RETRY_TOTAL_MAX) throw e
      if (e.status === 503) {
        if (busy >= RETRY_503_MAX) throw e
        busy++
      } else if (e.status === 429) {
        if (rateLimited >= RETRY_MAX) throw e
        rateLimited++
      } else throw e
      retries++
      await untilCancelled(sleep(Math.min(e.retryAfter ?? 3, RETRY_WAIT_MAX_S) * 1000), signal)
    }
  }
}

export function uploadRetryable(e: unknown): boolean {
  if (e instanceof PhotoUnreadableError) return false
  return !(e instanceof ShareHttpError && [404, 413, 415, 507].includes(e.status))
}

export function uploadErrorText(e: unknown): string {
  if (e instanceof PhotoUnreadableError) return t('這張照片打不開，請換一張或改用截圖')
  if (e instanceof ShareHttpError) {
    if (e.status === 413) return t('這個檔案太大了')
    if (e.status === 415) return t('這種檔案沒辦法傳')
    if (e.status === 507) return t('空間滿了，請跟分享給你的人說一聲')
    if (e.status === 404) return t('這個分享連結已失效')
  }
  return t('這張沒傳上去')
}
