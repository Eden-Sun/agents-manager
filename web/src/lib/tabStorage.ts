/**
 * 「這個分頁自己的」狀態（開著哪個對話、哪個 shell）。sessionStorage 每個分頁一份，重整回到同一個地方；
 * localStorage 是同一個瀏覽器的分頁共用的，只拿來當新開分頁的起點——不然分頁 A 重整會跳到分頁 B 最後選的 bot。
 * 讀的時候依序試（分頁自己的優先），值解析不了或讀不到就換下一個；寫的時候兩邊都寫。每一次讀寫都包 try/catch：隱私模式會丟例外。
 */
export interface KeyValueStore {
  getItem(key: string): string | null
  setItem(key: string, value: string): void
  removeItem(key: string): void
}

/** 分頁自己清掉的記號（寫在分頁自己那一份）：看到它就是「這一頁沒有」，不退回共用的（別的分頁可能還在寫）。 */
const CLEARED = '\u0000cleared'

export function readTabValue<T>(
  key: string,
  parse: (raw: string) => T | null,
  stores: readonly (KeyValueStore | null | undefined)[],
): T | null {
  for (const s of stores) {
    try {
      const raw = s?.getItem(key)
      if (raw === CLEARED) return null
      if (!raw) continue
      const v = parse(raw)
      if (v !== null) return v
    } catch {
      /* 這個來源讀不到或壞掉：換下一個 */
    }
  }
  return null
}

/** `stores[0]` 一律是分頁自己的（見上面）：清空時留下記號，其餘的（共用）直接移除。 */
export function writeTabValue(key: string, value: string | null, stores: readonly (KeyValueStore | null | undefined)[]): void {
  stores.forEach((s, i) => {
    try {
      if (value !== null) s?.setItem(key, value)
      else if (i === 0) s?.setItem(key, CLEARED)
      else s?.removeItem(key)
    } catch {
      /* 寫不進去就算了：這一頁還記得，重整就沒了 */
    }
  })
}

/** 瀏覽器的 [sessionStorage, localStorage]；取不到（沒有這個全域、被擋）的那一個是 null。 */
export function browserTabStores(): [KeyValueStore | null, KeyValueStore | null] {
  const pick = (name: 'sessionStorage' | 'localStorage'): KeyValueStore | null => {
    try {
      return ((globalThis as Record<string, unknown>)[name] as KeyValueStore | undefined) ?? null
    } catch {
      return null
    }
  }
  return [pick('sessionStorage'), pick('localStorage')]
}
