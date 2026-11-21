
export const MAX_EDGE = 1568
export const JPEG_QUALITY = 0.85

type OutMime = 'image/jpeg' | 'image/png'

export function compressible(mime: string): boolean {
  return mime === 'image/jpeg' || mime === 'image/png' || mime === 'image/webp'
}

export function fitSize(width: number, height: number, max = MAX_EDGE): { width: number; height: number } {
  const edge = Math.max(width, height)
  if (edge <= max || edge <= 0) return { width, height }
  const k = max / edge
  return { width: Math.max(1, Math.round(width * k)), height: Math.max(1, Math.round(height * k)) }
}

export function outputMime(input: string, hasAlpha: boolean): OutMime {
  if (input === 'image/jpeg') return 'image/jpeg'
  if (input === 'image/png') return 'image/png'
  return hasAlpha ? 'image/png' : 'image/jpeg'
}

export function hasTransparency(rgba: ArrayLike<number>): boolean {
  for (let i = 3; i < rgba.length; i += 4) if (rgba[i] < 255) return true
  return false
}

export function renameFor(name: string, mime: OutMime): string {
  const ext = mime === 'image/png' ? '.png' : '.jpg'
  const base = name.replace(/\.[^./\\]*$/, '')
  if (mime === 'image/jpeg' && /\.jpe?g$/i.test(name)) return name
  if (mime === 'image/png' && /\.png$/i.test(name)) return name
  return (base || 'image') + ext
}

export function keepCompressed(originalBytes: number, compressedBytes: number): boolean {
  return compressedBytes > 0 && compressedBytes < originalBytes
}

export async function compressImage(file: File): Promise<File> {
  if (!compressible(file.type) || typeof createImageBitmap !== 'function' || typeof document === 'undefined') return file
  let bmp: ImageBitmap | null = null
  try {
    bmp = await createImageBitmap(file, { imageOrientation: 'from-image' })
    const size = fitSize(bmp.width, bmp.height)
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
