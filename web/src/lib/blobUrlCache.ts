/**
 * 有使用者計數的 blob URL 快取（#2074）。
 *
 * token 帶不進 `<img src>`，圖要先抓成 blob URL；同一個 key 只抓一次。上限只算**沒人在用**的筆數：
 * 擠出時只 revoke 沒有元件持有的那筆。舊寫法擠出就 revoke，一次掛上超過上限的元件時，
 * 最早那幾個在 `setState(url)` 與 React commit 之間 URL 就失效了，`<img>` 載不出來（破圖）。
 *
 * 元件掛上時 `acquire`，卸掉時呼叫回傳的 `release`；沒人用的筆照最近用過的順序留著，超過上限才 revoke。
 */
export type BlobLease = { url: Promise<string>; release: () => void }

type Entry = { url: Promise<string>; users: number }

export function createBlobUrlCache(max: number) {
  const cache = new Map<string, Entry>()
  const revoke = (e: Entry) => void e.url.then((u) => URL.revokeObjectURL(u), () => {})

  function trim() {
    let idle = 0
    for (const e of cache.values()) if (e.users === 0) idle++
    // Map 照插入順序迭代：最前面的就是最久沒用的。
    for (const [key, e] of cache) {
      if (idle <= max) break
      if (e.users > 0) continue
      cache.delete(key)
      revoke(e)
      idle--
    }
  }

  function acquire(key: string, load: () => Promise<string>): BlobLease {
    let entry = cache.get(key)
    if (entry) {
      // 重新插入就是「最近用過」。
      cache.delete(key)
    } else {
      const url = load()
      const created: Entry = { url, users: 0 }
      entry = created
      // 失敗的不留在快取裡，下次再試；clear 之後同 key 可能已放進新的請求，只刪自己這一筆。
      url.catch(() => {
        if (cache.get(key) === created) cache.delete(key)
      })
    }
    cache.set(key, entry)
    entry.users++
    const held = entry
    let released = false
    return {
      url: entry.url,
      release: () => {
        if (released) return
        released = true
        held.users--
        // clear 之後才卸掉的舊筆：clear 已 revoke，不在快取裡，不用再處理。
        if (cache.get(key) === held) trim()
      },
    }
  }

  /** 全部 revoke 並清空（換 bot／離開時）。 */
  function clear() {
    for (const e of cache.values()) revoke(e)
    cache.clear()
  }

  return { acquire, clear, size: () => cache.size }
}
