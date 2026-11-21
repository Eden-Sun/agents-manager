import type { ShareFile } from './shareModel'

const IMAGE_RE = /\.(svg|png|jpe?g|gif|webp)$/i

export const isImageName = (name: string): boolean => IMAGE_RE.test(name)
export const isSvgName = (name: string): boolean => /\.svg$/i.test(name)
export const pngName = (name: string): string => `${name.replace(/\.svg$/i, '')}.png`

export const displayName = (name: string): string => (isImageName(name) ? name.replace(IMAGE_RE, '') || name : name)

const at = (s: string | null | undefined): number => {
  const t = s ? Date.parse(s) : NaN
  return Number.isFinite(t) ? t : NaN
}

const SKEW_MS = 2000

export function imagesByMessage(
  messages: readonly { id: string; role: string; text: string; created_at: string }[],
  files: readonly ShareFile[],
): Map<string, ShareFile[]> {
  const out = new Map<string, ShareFile[]>()
  const add = (id: string, f: ShareFile) => {
    const list = out.get(id) ?? []
    if (list.length < 6 && !list.some((x) => x.name === f.name)) list.push(f)
    out.set(id, list)
  }
  const imgs = files.filter((f) => isImageName(f.name))
  for (const f of imgs) {
    const t = at(f.modified_at)
    if (!Number.isFinite(t)) continue
    const next = messages.findIndex((m) => (m.role === 'assistant' ? at(m.created_at) >= t - SKEW_MS : at(m.created_at) > t))
    if (next >= 0 && messages[next].role === 'assistant') {
      add(messages[next].id, f)
      continue
    }
    const prev = messages[(next < 0 ? messages.length : next) - 1]
    if (prev?.role === 'assistant') add(prev.id, f)
  }
  const names = imgs.map((f) => f.name)
  for (const m of messages) {
    if (m.role !== 'assistant') continue
    for (const f of imgs) if (mentions(m.text, f.name, names)) add(m.id, f)
  }
  return out
}

function mentions(text: string, name: string, others: readonly string[]): boolean {
  const longer = others.filter((o) => o.length > name.length && o.includes(name))
  for (let i = text.indexOf(name); i >= 0; i = text.indexOf(name, i + 1)) {
    const covered = longer.some((o) => {
      for (let k = o.indexOf(name); k >= 0; k = o.indexOf(name, k + 1)) {
        if (i - k >= 0 && text.startsWith(o, i - k)) return true
      }
      return false
    })
    if (!covered) return true
  }
  return false
}

export function svgExternalRefs(text: string): boolean {
  for (const m of text.matchAll(/\b(?:xlink:)?href\s*=\s*(["'])([^"']*)\1/gi)) {
    const v = m[2].trim()
    if (!v.startsWith('#') && !/^data:/i.test(v)) return true
  }
  if (/url\(\s*(["']?)\s*(?!#|data:)[^)\s"']/i.test(text)) return true
  return /@import\b/i.test(text) || /<foreignObject\b/i.test(text)
}

const num = (v: string | undefined): number | null => {
  if (!v) return null
  const m = v.trim().match(/^([0-9]*\.?[0-9]+)(px)?$/i)
  const n = m ? Number(m[1]) : NaN
  return Number.isFinite(n) && n > 0 ? n : null
}

export function svgSize(text: string): { w: number; h: number } | null {
  const tag = text.match(/<svg\b[^>]*>/i)?.[0]
  if (!tag) return null
  const attr = (n: string) => tag.match(new RegExp(`\\s${n}\\s*=\\s*["']([^"']*)["']`, 'i'))?.[1]
  const w = num(attr('width'))
  const h = num(attr('height'))
  const vb = attr('viewBox')
    ?.trim()
    .split(/[\s,]+/)
    .map(Number)
  const vw = vb && vb.length === 4 && vb[2] > 0 ? vb[2] : null
  const vh = vb && vb.length === 4 && vb[3] > 0 ? vb[3] : null
  if (w && h) return { w, h }
  if (w && vw && vh) return { w, h: (w * vh) / vw }
  if (h && vw && vh) return { w: (h * vw) / vh, h }
  if (vw && vh) return { w: vw, h: vh }
  return null
}

export function svgWithSize(text: string, size: { w: number; h: number }): string {
  return text.replace(/<svg\b[^>]*>/i, (tag) => {
    let t = tag.replace(/\s(width|height)\s*=\s*(["'])[^"']*\2/gi, '')
    t = t.replace(/^<svg\b/i, `<svg width="${size.w}" height="${size.h}"`)
    return t
  })
}

export function pngCanvasSize(w: number, h: number, scale = 2): { w: number; h: number } {
  const s = Math.min(scale, 8192 / w, 8192 / h, Math.sqrt(16_000_000 / (w * h)))
  return { w: Math.max(1, Math.floor(w * s)), h: Math.max(1, Math.floor(h * s)) }
}

export function svgWellFormed(text: string): boolean {
  if (typeof DOMParser === 'undefined') return true
  try {
    return new DOMParser().parseFromString(text, 'image/svg+xml').getElementsByTagName('parsererror').length === 0
  } catch {
    return false
  }
}

export class SvgTaintedError extends Error {
  constructor() {
    super('svg_tainted')
  }
}

export async function svgToPng(svgText: string, scale = 2): Promise<Blob> {
  if (svgExternalRefs(svgText)) throw new SvgTaintedError()
  const size = svgSize(svgText) ?? { w: 1080, h: 1080 }
  const url = URL.createObjectURL(new Blob([svgWithSize(svgText, size)], { type: 'image/svg+xml' }))
  try {
    const img = new Image()
    await new Promise<void>((resolve, reject) => {
      img.onload = () => resolve()
      img.onerror = () => reject(new Error('svg_decode'))
      img.src = url
    })
    const out = pngCanvasSize(size.w, size.h, scale)
    const canvas = document.createElement('canvas')
    canvas.width = out.w
    canvas.height = out.h
    const ctx = canvas.getContext('2d')
    if (!ctx) throw new Error('no_canvas')
    ctx.drawImage(img, 0, 0, out.w, out.h)
    return await new Promise<Blob>((resolve, reject) => {
      try {
        canvas.toBlob((b) => (b ? resolve(b) : reject(new Error('png_encode'))), 'image/png')
      } catch (e) {
        reject(e instanceof DOMException && e.name === 'SecurityError' ? new SvgTaintedError() : e)
      }
    })
  } finally {
    URL.revokeObjectURL(url)
  }
}

export function saveBlob(blob: Blob, name: string): void {
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = name
  a.rel = 'noopener'
  document.body.appendChild(a)
  a.click()
  a.remove()
  setTimeout(() => URL.revokeObjectURL(url), 30_000)
}

type ShareNav = Navigator & { canShare?: (d: { files: File[] }) => boolean }

export function canShareFile(file: File): boolean {
  const nav = (typeof navigator === 'undefined' ? undefined : navigator) as ShareNav | undefined
  const coarse = typeof window !== 'undefined' && typeof window.matchMedia === 'function' && window.matchMedia('(pointer: coarse)').matches
  return coarse && typeof nav?.share === 'function' && nav.canShare?.({ files: [file] }) === true
}

export async function shareOrSave(blob: Blob, name: string, type: string): Promise<'shared' | 'saved' | 'cancelled'> {
  const file = new File([blob], name, { type })
  if (canShareFile(file)) {
    try {
      await navigator.share({ files: [file] })
      return 'shared'
    } catch (e) {
      if (e instanceof DOMException && e.name === 'AbortError') return 'cancelled'
    }
  }
  saveBlob(blob, name)
  return 'saved'
}
