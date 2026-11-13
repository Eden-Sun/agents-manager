import * as api from '../api'
import { createBlobUrlCache, type BlobLease } from './blobUrlCache'

/**
 * 「bot 給你的檔案」裡的圖檔預覽（使用者 2026-09-18）：桌面滑過浮出、手機托盤縮圖共用。
 *
 * token 帶不進 `<img src>`，跟下載一樣先抓成 blob URL。同一個檔（名字＋修改時間）只抓一次。
 * 元件掛上時 `blobFor` 拿租約、卸掉時 `release`；還有元件在用的不會被 revoke，
 * 沒人用的最多留 12 張（#2074：手機托盤一次畫超過 12 張縮圖時，舊寫法擠出就 revoke，最前面幾張破圖）。
 * 換 bot 由呼叫端 `clearOutboxPreviews()` 全部釋放。
 */
const MAX_IDLE = 12
const cache = createBlobUrlCache(MAX_IDLE)

/**
 * 同時在飛的取檔數上限。遠端主機的檔案每張都是一條 ssh，幾十張縮圖一起發會塞滿瀏覽器連線（HTTP/1.1 每源 6 條）
 * 與 daemon 的 ssh 名額；而且 `blobUrl` 的 30 秒逾時從呼叫那刻起算，排在後面的圖光排隊就逾時，整批 ✕。
 * 限流放在呼叫 api 之前，逾時只從真的送出才開始算。
 */
export const MAX_INFLIGHT = 4
export function createLimiter(max: number) {
  let running = 0
  const waiting: Array<() => void> = []
  const next = () => {
    while (running < max && waiting.length > 0) {
      running++
      waiting.shift()!()
    }
  }
  return function limited<T>(task: () => Promise<T>): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      waiting.push(() => {
        task().then(resolve, reject).finally(() => {
          running--
          next()
        })
      })
      next()
    })
  }
}
const limited = createLimiter(MAX_INFLIGHT)
const defaultLoad = (botId: string, name: string) => limited(() => api.outboxFileUrl(botId, name))

export function blobFor(
  botId: string,
  name: string,
  modified: number,
  load: (botId: string, name: string) => Promise<string> = defaultLoad,
): BlobLease {
  return cache.acquire(`${botId} ${name} ${modified}`, () => load(botId, name))
}

export function clearOutboxPreviews() {
  cache.clear()
}
