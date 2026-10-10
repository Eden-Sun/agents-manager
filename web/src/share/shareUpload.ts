/**
 * 分享頁的上傳（SPEC §20.4）：一張接一張傳、遇到 429／503 照 `Retry-After` 自己等著重試，手機照片先轉成 bot 讀得到的 JPEG。
 *
 * - 客訴 2026-10-04：長輩一次選好幾張照片，原本同時全部送出，第三張起就是「傳得太快了」。現在依序傳，「太快」不丟給使用者。
 *   入口「暫時忙」回 503（同樣帶 `Retry-After`）也照樣等了再試，次數另外算、比較少（見 [`RETRY_503_MAX`]）。
 * - 受限 bot 只能用 Read 看圖，claude 的 Read 只認 png／jpg／jpeg／gif／webp（2.1.289 binary：`new Set(["png","jpg","jpeg","gif","webp"])`），
 *   iPhone／部分 Android 的 HEIC 它讀不了，daemon 也不收。所以 HEIC 在瀏覽器裡轉成 JPEG；JPEG 照片長邊超過 2048 也縮一下——
 *   模型看圖本來就會縮到更小，原尺寸 3–8 MB 的照片只是讓長輩在手機網路上等更久、更快塞滿 `inbox/` 的 200 MB。
 *   PNG／GIF／WebP（截圖、貼圖）原樣送：轉 JPEG 會糊掉截圖上的字。
 */
import type { ShareClient } from './shareApi'
import { SHARE_FILE_MAX, ShareHttpError } from './shareModel'

/** 照片縮到長邊這麼長。 */
export const PHOTO_MAX_EDGE = 2048
const JPEG_QUALITY = 0.85
/** 429／503 最多重試幾次、單次最多等幾秒（`Retry-After` 沒給時等 3 秒）。 */
const RETRY_MAX = 12
const RETRY_WAIT_MAX_S = 30
/** 503 也等了重試，但只試這麼多次：遠端主機真的離線時不要讓人盯著「上傳中」太久。 */
const RETRY_503_MAX = 3

/** 瀏覽器解不開的 HEIC（多半是電腦上的 Chrome）：bot 也讀不了，請使用者換一張。 */
export class PhotoUnreadableError extends Error {
  constructor() {
    super('photo_unreadable')
  }
}

const isHeic = (f: File) => /^image\/hei[cf]/i.test(f.type) || /\.hei[cf]s?$/i.test(f.name)
const isJpeg = (f: File) => /^image\/jpe?g$/i.test(f.type) || /\.jpe?g$/i.test(f.name)

function jpegName(name: string): string {
  const base = name.replace(/\.[^./\\]*$/, '')
  return `${base || 'photo'}.jpg`
}

export interface ImageCodec {
  /** 解開圖片（套用 EXIF 方向）；解不開回 null。 */
  decode(f: File): Promise<{ width: number; height: number; source: CanvasImageSource; close(): void } | null>
  /** 畫成指定大小的 JPEG。 */
  encodeJpeg(source: CanvasImageSource, width: number, height: number, quality: number): Promise<Blob | null>
}

const browserCodec: ImageCodec = {
  async decode(f) {
    if (typeof createImageBitmap !== 'function') return null
    try {
      // `imageOrientation: 'from-image'` 是預設：手機直拍的照片不會躺著。
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
    // 透明的地方鋪白，不要變黑。
    ctx.fillStyle = '#fff'
    ctx.fillRect(0, 0, width, height)
    ctx.drawImage(source, 0, 0, width, height)
    return new Promise((resolve) => canvas.toBlob((b) => resolve(b), 'image/jpeg', quality))
  },
}

/** 要送出去的檔：HEIC 一律轉 JPEG；JPEG 長邊超過 [`PHOTO_MAX_EDGE`] 縮小；其他原樣。 */
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
    // 縮完反而比較大（本來就壓得很小的 JPEG）就送原檔。
    if (!heic && blob.size >= f.size) return f
    return new File([blob], heic ? jpegName(f.name) : f.name.replace(/\.jpeg$/i, '.jpg'), { type: 'image/jpeg', lastModified: f.lastModified })
  } finally {
    img.close()
  }
}

export interface UploadOptions {
  /** 等多久（測試換掉）。 */
  sleep?: (ms: number) => Promise<void>
  codec?: ImageCodec
}

const defaultSleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms))

/** 先整理照片，再上傳；429／503 照 `Retry-After` 等了重試（429 最多 [`RETRY_MAX`] 次，503 最多 [`RETRY_503_MAX`] 次），其他錯誤照拋。 */
export async function uploadPatiently(client: ShareClient, f: File, opts: UploadOptions = {}): Promise<{ id: string; name: string }> {
  const sleep = opts.sleep ?? defaultSleep
  const file = await preparePhoto(f, opts.codec)
  // 照片縮過才量：原尺寸超過 25 MB 的照片縮完多半就放得下。
  if (file.size > SHARE_FILE_MAX) throw new ShareHttpError(413)
  let busy = 0
  for (let attempt = 0; ; attempt++) {
    try {
      return await client.upload(file)
    } catch (e) {
      if (!(e instanceof ShareHttpError) || attempt >= RETRY_MAX) throw e
      if (e.status === 503) {
        if (busy++ >= RETRY_503_MAX) throw e
      } else if (e.status !== 429) throw e
      await sleep(Math.min(e.retryAfter ?? 3, RETRY_WAIT_MAX_S) * 1000)
    }
  }
}

/** 再傳一次可能會好（網路、暫時的忙碌）；打不開的照片、太大、不收的種類再傳也一樣。 */
export function uploadRetryable(e: unknown): boolean {
  if (e instanceof PhotoUnreadableError) return false
  return !(e instanceof ShareHttpError && [404, 413, 415, 507].includes(e.status))
}

/** 給長輩看的上傳錯誤：簡單、沒有技術字。 */
export function uploadErrorText(e: unknown): string {
  if (e instanceof PhotoUnreadableError) return '這張照片打不開，請換一張或改用截圖'
  if (e instanceof ShareHttpError) {
    if (e.status === 413) return '這個檔案太大了'
    if (e.status === 415) return '這種檔案沒辦法傳'
    if (e.status === 507) return '空間滿了，請跟分享給你的人說一聲'
    if (e.status === 404) return '這個分享連結已失效'
  }
  return '這張沒傳上去'
}
