import type { ShareClient } from './shareApi'
import { svgExternalRefs, SvgTaintedError, svgToPng, svgWellFormed } from './shareImage'
import type { ShareFile } from './shareModel'

/**
 * `external`＝圖引用外部資源（為了讀者的隱私不轉、不分享，永遠不會好）；`broken`＝這一版載不出來（bot 把圖寫壞了、網路斷了），
 * bot 修好、檔案換版就會重試——兩種講法不同（使用者 2026-10-04：長輩看到「沒辦法分享」以為圖不能分享）。
 */
export type PngResult = { blob: Blob } | 'external' | 'broken'

/** 圖片快取的上限（#1175）：分享頁可以開好幾天，bot 一直改圖、換名字，快取不能只長不縮；超過就淘汰最久沒用的。 */
export const SHARE_IMAGE_CACHE_LIMITS = {
  png: { maxEntries: 32, maxBytes: 64 * 1024 * 1024 },
  raster: { maxEntries: 24, maxBytes: 96 * 1024 * 1024 },
}

type CacheEntry<V> = { name: string; bytes: number; value: Promise<V> }

/**
 * 一個 client 的圖片快取，key 是「檔名＋版本」（#1175）：
 * - 同名的舊版本一進新版就淘汰（`rasterOf` 原本就這樣，`pngOf` 以前沒有，改版 100 次就留 100 份 PNG）；
 * - 條數與估計位元組（Blob.size）有上限，超過淘汰最久沒用的（Map 的插入順序就是 LRU 順序）；
 * - 抓取失敗（reject）不留，下次再抓；成功的留到被淘汰，或清單裡已經沒有那個檔（`prune`）。
 * 舊版的抓取晚回來時只看自己還在不在表裡，不會把舊結果塞回去，也不會蓋掉新版。
 */
class ImageCache<V> {
  private readonly entries = new Map<string, CacheEntry<V>>()
  private readonly limits: { maxEntries: number; maxBytes: number }
  private readonly sizeOf: (v: V) => number

  constructor(limits: { maxEntries: number; maxBytes: number }, sizeOf: (v: V) => number) {
    this.limits = limits
    this.sizeOf = sizeOf
  }

  get(name: string, version: string, load: () => Promise<V>): Promise<V> {
    const key = JSON.stringify([name, version])
    const hit = this.entries.get(key)
    if (hit) {
      // 用過就移到最後。
      this.entries.delete(key)
      this.entries.set(key, hit)
      return hit.value
    }
    for (const [k, e] of this.entries) if (e.name === name) this.entries.delete(k)
    const entry: CacheEntry<V> = {
      name,
      bytes: 0,
      value: load().then(
        (v) => {
          if (this.entries.get(key) === entry) {
            entry.bytes = this.sizeOf(v)
            this.trim(key)
          }
          return v
        },
        (e: unknown) => {
          if (this.entries.get(key) === entry) this.entries.delete(key)
          throw e
        },
      ),
    }
    this.entries.set(key, entry)
    return entry.value
  }

  /** 超出上限就由舊到新淘汰；剛存進來的（`keep`）不淘汰，免得連當前這一版都留不住。 */
  private trim(keep: string) {
    const total = () => {
      let t = 0
      for (const e of this.entries.values()) t += e.bytes
      return t
    }
    for (const k of [...this.entries.keys()]) {
      if (this.entries.size <= this.limits.maxEntries && total() <= this.limits.maxBytes) return
      if (k !== keep) this.entries.delete(k)
    }
  }

  prune(live: ReadonlySet<string>) {
    for (const [k, e] of this.entries) if (!live.has(e.name)) this.entries.delete(k)
  }
}

function cacheOf<V>(map: WeakMap<ShareClient, ImageCache<V>>, client: ShareClient, make: () => ImageCache<V>): ImageCache<V> {
  let c = map.get(client)
  if (!c) map.set(client, (c = make()))
  return c
}

/** 向量圖 → 點陣圖每個檔（同版本）只轉一次，清單、對話、放大檢視共用。一出現就先轉好：`navigator.share` 要在點擊的
 *  同一個手勢裡呼叫，不能點了才開始轉。抓不到檔（網路）不留快取，下次再試。 */
const pngCache = new WeakMap<ShareClient, ImageCache<PngResult>>()

export function pngOf(client: ShareClient, name: string, version: string): Promise<PngResult> {
  return cacheOf(pngCache, client, () => new ImageCache<PngResult>(SHARE_IMAGE_CACHE_LIMITS.png, (r) => (typeof r === 'object' ? r.blob.size : 0)))
    .get(name, version, () =>
      client.fileBlob(name).then(async (b): Promise<PngResult> => {
        const text = await b.text()
        // 寫壞的先講「還在修」：bot 修好、換版就會重試（daemon 也會提醒它修，SPEC §20）。
        if (!svgWellFormed(text)) return 'broken'
        if (svgExternalRefs(text)) return 'external'
        try {
          return { blob: await svgToPng(text) }
        } catch (e) {
          return e instanceof SvgTaintedError ? 'external' : 'broken'
        }
      }),
    )
    .catch((): PngResult => 'broken')
}


/** 點陣圖的整檔每個檔（同版本）只抓一次，清單、對話、放大檢視共用。抓不到不留快取，下次再試。整張圖的 Blob 留著就是記憶體，所以有上限。 */
const rasterCache = new WeakMap<ShareClient, ImageCache<Blob>>()

export function rasterOf(client: ShareClient, name: string, version: string): Promise<Blob> {
  return cacheOf(rasterCache, client, () => new ImageCache<Blob>(SHARE_IMAGE_CACHE_LIMITS.raster, (b) => b.size)).get(name, version, () => client.fileBlob(name))
}

/** 清單更新後呼叫：已經不在清單裡的檔（刪掉、改名）的圖片快取整條淘汰（#1175）。 */
export function pruneShareImages(client: ShareClient, files: ShareFile[]): void {
  const live = new Set(files.map((f) => f.name))
  pngCache.get(client)?.prune(live)
  rasterCache.get(client)?.prune(live)
}
