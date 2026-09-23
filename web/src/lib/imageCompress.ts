/**
 * 圖片附件上傳前在瀏覽器縮圖（2026-09-23 使用者：「圖檔前端是否已經壓縮節省 token」）。
 *
 * - **長邊上限 1568px**：Claude 視覺模型的有效解析度，超過的 CLI／API 本來就會縮，只是白白傳輸與落地。
 *   token 算的是像素（約 寬×高／750），不是位元組——省 token 的是縮尺寸，換格式只省傳輸與磁碟。
 * - **格式**：JPEG 進 JPEG 出（q 0.85）。PNG 一律維持 PNG、只縮尺寸：PNG 附件幾乎都是截圖，
 *   文字與 UI 邊緣壓成 JPEG 會起振鈴、糊字，而 token 又不因 JPEG 變少；透明度也只有 PNG 留得住。
 *   WebP（瀏覽器不一定編得出 WebP）不透明時出 JPEG、有透明時出 PNG。
 * - **EXIF 方向**：用 `createImageBitmap(…, { imageOrientation: 'from-image' })` 把方向烤進像素，
 *   重新編碼後不再帶 EXIF，手機直拍的照片不會轉歪。
 * - 壓完沒有比原檔小（已經很小的 JPEG、尺寸本來就在上限內的 PNG）就用原檔。GIF（動畫）、SVG、HEIC 等其他格式與非圖片一律不動。
 * - 解不開、瀏覽器沒有 canvas 就用原檔：壓縮是省錢，不能讓附件因此傳不出去。
 */

export const MAX_EDGE = 1568
export const JPEG_QUALITY = 0.85

type OutMime = 'image/jpeg' | 'image/png'

/** 會嘗試壓縮的輸入格式；其他（含 GIF 動畫）原樣上傳。 */
export function compressible(mime: string): boolean {
  return mime === 'image/jpeg' || mime === 'image/png' || mime === 'image/webp'
}

/** 等比縮到長邊不超過 `max`；本來就在上限內回原尺寸。 */
export function fitSize(width: number, height: number, max = MAX_EDGE): { width: number; height: number } {
  const edge = Math.max(width, height)
  if (edge <= max || edge <= 0) return { width, height }
  const k = max / edge
  return { width: Math.max(1, Math.round(width * k)), height: Math.max(1, Math.round(height * k)) }
}

/** 輸出格式：JPEG→JPEG；PNG→PNG；WebP 看有沒有透明。 */
export function outputMime(input: string, hasAlpha: boolean): OutMime {
  if (input === 'image/jpeg') return 'image/jpeg'
  if (input === 'image/png') return 'image/png'
  return hasAlpha ? 'image/png' : 'image/jpeg'
}

/** RGBA 像素裡有沒有不透明度 < 255 的點。 */
export function hasTransparency(rgba: ArrayLike<number>): boolean {
  for (let i = 3; i < rgba.length; i += 4) if (rgba[i] < 255) return true
  return false
}

/** 換了格式就換副檔名，agent 看檔名判斷格式時才不會對不上。 */
export function renameFor(name: string, mime: OutMime): string {
  const ext = mime === 'image/png' ? '.png' : '.jpg'
  const base = name.replace(/\.[^./\\]*$/, '')
  if (mime === 'image/jpeg' && /\.jpe?g$/i.test(name)) return name
  if (mime === 'image/png' && /\.png$/i.test(name)) return name
  return (base || 'image') + ext
}

/** 壓出來的比原檔小才用，否則用原檔。 */
export function keepCompressed(originalBytes: number, compressedBytes: number): boolean {
  return compressedBytes > 0 && compressedBytes < originalBytes
}

/** 上傳前的縮圖；任何一步失敗或沒有變小都回原檔。 */
export async function compressImage(file: File): Promise<File> {
  if (!compressible(file.type) || typeof createImageBitmap !== 'function' || typeof document === 'undefined') return file
  let bmp: ImageBitmap | null = null
  try {
    bmp = await createImageBitmap(file, { imageOrientation: 'from-image' })
    const size = fitSize(bmp.width, bmp.height)
    // PNG 尺寸在上限內：只能換成同一份 PNG，不會更小，也不動像素。
    if (file.type === 'image/png' && size.width === bmp.width && size.height === bmp.height) return file
    const canvas = document.createElement('canvas')
    canvas.width = size.width
    canvas.height = size.height
    const ctx = canvas.getContext('2d')
    if (!ctx) return file
    ctx.imageSmoothingQuality = 'high'
    ctx.drawImage(bmp, 0, 0, size.width, size.height)
    const alpha = file.type === 'image/webp' && hasTransparency(ctx.getImageData(0, 0, size.width, size.height).data)
    const mime = outputMime(file.type, alpha)
    if (mime === 'image/jpeg' && file.type !== 'image/jpeg') {
      // 不透明的 WebP 轉 JPEG：先鋪白底，免得半透明邊緣變黑。
      ctx.globalCompositeOperation = 'destination-over'
      ctx.fillStyle = '#fff'
      ctx.fillRect(0, 0, size.width, size.height)
    }
    const blob = await new Promise<Blob | null>((res) => canvas.toBlob(res, mime, mime === 'image/jpeg' ? JPEG_QUALITY : undefined))
    if (!blob || blob.type !== mime || !keepCompressed(file.size, blob.size)) return file
    return new File([blob], renameFor(file.name || 'image', mime), { type: mime, lastModified: file.lastModified })
  } catch {
    return file
  } finally {
    bmp?.close()
  }
}
