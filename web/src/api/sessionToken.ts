/**
 * 這台裝置的 UI token。
 *
 * 為什麼要存：`GET /api/session` 自 SPEC §7.1a 起**只對 loopback** 直接發 token，其他裝置（手機走區網／
 * Tailscale）得用一次性配對碼換。不存起來的話，重整一次、或分頁被背景回收一次，就要再配對一次——
 * 「已經配對過的裝置不要被踢出去」是這條的前提。
 */

export const TOKEN_KEY = 'am.session.token'

export interface TokenStore {
  get(): string
  set(token: string): void
  clear(): void
}

/**
 * localStorage 版。私密視窗、封鎖 site data、或截圖預覽下讀寫都可能直接丟例外，
 * 所以每一步都包起來，並留一份只活這一頁的鏡像：存不下去至少不要整個 App 開不起來。
 */
export function deviceTokenStore(): TokenStore {
  let mirror = ''
  return {
    get() {
      try {
        return localStorage.getItem(TOKEN_KEY) ?? mirror
      } catch {
        return mirror
      }
    },
    set(token) {
      mirror = token
      try {
        localStorage.setItem(TOKEN_KEY, token)
      } catch {
        /* 存不了就只活這一頁 */
      }
    },
    clear() {
      mirror = ''
      try {
        localStorage.removeItem(TOKEN_KEY)
      } catch {
        /* 同上 */
      }
    },
  }
}

/** 測試與「不要落地」的場合用。 */
export function memoryTokenStore(initial = ''): TokenStore {
  let token = initial
  return {
    get: () => token,
    set: (t) => {
      token = t
    },
    clear: () => {
      token = ''
    },
  }
}
