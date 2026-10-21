/**
 * daemon 自動裝了某台的 grok／antigravity-cli herdr integration（WS `herdr_integration_auto_install`，issue #1871）。
 * 每台每家每個 daemon 行程最多一次，所以 `host:name` 就是去重的鍵：重連重播同一筆不再跳第二次 toast。
 */
import { isRec, pick, str } from '../api/normalize'

const seen = new Set<string>()

export function autoInstallNotice(data: unknown): { kind: 'info' | 'error'; text: string } | null {
  if (!isRec(data)) return null
  const host = str(pick(data, 'host'))
  const name = str(pick(data, 'name'))
  if (!host || !name) return null
  const key = `${host}:${name}`
  if (seen.has(key)) return null
  seen.add(key)
  const where = host === 'local' ? '本機' : host
  if (pick(data, 'ok') === true) return { kind: 'info', text: `已在 ${where} 自動安裝 herdr ${name} integration` }
  const error = str(pick(data, 'error')) || '原因不明'
  return { kind: 'error', text: `在 ${where} 自動安裝 herdr ${name} integration 失敗：${error}（不重試；到設定頁「更新檢查」按安裝再試，或重啟 daemon）`.slice(0, 500) }
}

export function resetAutoInstallNoticesForTest() {
  seen.clear()
}
