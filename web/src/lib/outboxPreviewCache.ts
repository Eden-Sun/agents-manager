import * as api from '../api'
import { createBlobUrlCache, type BlobLease } from './blobUrlCache'

const MAX_IDLE = 12
const cache = createBlobUrlCache(MAX_IDLE)

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
