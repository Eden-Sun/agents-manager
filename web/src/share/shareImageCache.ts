import type { ShareClient } from './shareApi'
import { svgExternalRefs, SvgTaintedError, svgToPng, svgWellFormed } from './shareImage'
import { ImageWorkSkipped, ShareImageWorkQueue, type ImageWork } from './shareImageQueue'
import type { ShareFile } from './shareModel'

export type PngResult = { blob: Blob } | 'external' | 'broken'

export const SHARE_IMAGE_CACHE_LIMITS = {
  png: { maxEntries: 32, maxBytes: 64 * 1024 * 1024 },
  raster: { maxEntries: 24, maxBytes: 96 * 1024 * 1024 },
}

type CacheEntry<V> = { name: string; bytes: number; value: Promise<V>; work: ImageWork<V>; consumers: number; settled: boolean }

export interface ShareImageRequest<V> {
  promise: Promise<V>
  release(): void
}

class ImageCache<V> {
  private readonly entries = new Map<string, CacheEntry<V>>()
  private readonly limits: { maxEntries: number; maxBytes: number }
  private readonly sizeOf: (v: V) => number
  private readonly queue: ShareImageWorkQueue
  private readonly namespace: string

  constructor(limits: { maxEntries: number; maxBytes: number }, sizeOf: (v: V) => number, queue: ShareImageWorkQueue, namespace: string) {
    this.limits = limits
    this.sizeOf = sizeOf
    this.queue = queue
    this.namespace = namespace
  }

  get(name: string, version: string, load: () => Promise<V>): Promise<V> {
    return this.request(name, version, load).value
  }

  watch(name: string, version: string, load: () => Promise<V>): ShareImageRequest<V> {
    const key = JSON.stringify([name, version])
    const entry = this.request(name, version, load)
    entry.consumers++
    let released = false
    return {
      promise: entry.value,
      release: () => {
        if (released) return
        released = true
        entry.consumers--
        if (entry.consumers === 0 && !entry.settled && entry.work.cancel()) this.remove(key, entry)
      },
    }
  }

  private request(name: string, version: string, load: () => Promise<V>): CacheEntry<V> {
    const key = JSON.stringify([name, version])
    const hit = this.entries.get(key)
    if (hit) {
      this.entries.delete(key)
      this.entries.set(key, hit)
      hit.work.promote()
      return hit
    }
    for (const [k, e] of this.entries) if (e.name === name) this.remove(k, e)
    const work = this.queue.schedule(JSON.stringify([this.namespace, name, version]), load)
    const entry: CacheEntry<V> = {
      name,
      bytes: 0,
      work,
      consumers: 0,
      settled: false,
      value: work.promise.then(
        (v) => {
          entry.settled = true
          if (this.entries.get(key) === entry) {
            entry.bytes = this.sizeOf(v)
            this.trim(key)
          }
          return v
        },
        (e: unknown) => {
          if (this.entries.get(key) === entry) this.remove(key, entry)
          throw e
        },
      ),
    }
    this.entries.set(key, entry)
    return entry
  }

  private trim(keep: string) {
    const total = () => {
      let t = 0
      for (const e of this.entries.values()) t += e.bytes
      return t
    }
    for (const k of [...this.entries.keys()]) {
      if (this.entries.size <= this.limits.maxEntries && total() <= this.limits.maxBytes) return
      if (k !== keep) {
        const entry = this.entries.get(k)
        if (entry?.settled) this.remove(k, entry)
      }
    }
  }

  prune(live: ReadonlySet<string>) {
    for (const [k, e] of this.entries) if (!live.has(e.name)) this.remove(k, e)
  }

  private remove(key: string, entry: CacheEntry<V>) {
    if (this.entries.get(key) !== entry) return
    this.entries.delete(key)
    entry.work.cancel()
  }
}

const imageQueues = new WeakMap<ShareClient, ShareImageWorkQueue>()

function queueOf(client: ShareClient): ShareImageWorkQueue {
  let queue = imageQueues.get(client)
  if (!queue) imageQueues.set(client, (queue = new ShareImageWorkQueue()))
  return queue
}

function cacheOf<V>(map: WeakMap<ShareClient, ImageCache<V>>, client: ShareClient, make: () => ImageCache<V>): ImageCache<V> {
  let c = map.get(client)
  if (!c) map.set(client, (c = make()))
  return c
}

const pngCache = new WeakMap<ShareClient, ImageCache<PngResult>>()

export function pngOf(client: ShareClient, name: string, version: string): Promise<PngResult> {
  return cacheOf(pngCache, client, () => new ImageCache<PngResult>(SHARE_IMAGE_CACHE_LIMITS.png, (r) => (typeof r === 'object' ? r.blob.size : 0), queueOf(client), 'png'))
    .get(name, version, () => loadPng(client, name))
    .catch((e): PngResult => {
      if (e instanceof ImageWorkSkipped) throw e
      return 'broken'
    })
}

function loadPng(client: ShareClient, name: string): Promise<PngResult> {
  return client.fileBlob(name).then(async (b): Promise<PngResult> => {
    const text = await b.text()
    if (!svgWellFormed(text)) return 'broken'
    if (svgExternalRefs(text)) return 'external'
    try {
      return { blob: await svgToPng(text) }
    } catch (e) {
      return e instanceof SvgTaintedError ? 'external' : 'broken'
    }
  })
}

export function pngRequest(client: ShareClient, name: string, version: string): ShareImageRequest<PngResult> {
  const request = cacheOf(pngCache, client, () => new ImageCache<PngResult>(SHARE_IMAGE_CACHE_LIMITS.png, (r) => (typeof r === 'object' ? r.blob.size : 0), queueOf(client), 'png'))
    .watch(name, version, () => loadPng(client, name))
  return {
    promise: request.promise.catch((e): PngResult => {
      if (e instanceof ImageWorkSkipped) throw e
      return 'broken'
    }),
    release: request.release,
  }
}

const rasterCache = new WeakMap<ShareClient, ImageCache<Blob>>()

export function rasterOf(client: ShareClient, name: string, version: string): Promise<Blob> {
  return cacheOf(rasterCache, client, () => new ImageCache<Blob>(SHARE_IMAGE_CACHE_LIMITS.raster, (b) => b.size, queueOf(client), 'raster'))
    .get(name, version, () => client.fileBlob(name))
}

export function rasterRequest(client: ShareClient, name: string, version: string): ShareImageRequest<Blob> {
  return cacheOf(rasterCache, client, () => new ImageCache<Blob>(SHARE_IMAGE_CACHE_LIMITS.raster, (b) => b.size, queueOf(client), 'raster'))
    .watch(name, version, () => client.fileBlob(name))
}

export function pruneShareImages(client: ShareClient, files: ShareFile[]): void {
  const live = new Set(files.map((f) => f.name))
  pngCache.get(client)?.prune(live)
  rasterCache.get(client)?.prune(live)
}
