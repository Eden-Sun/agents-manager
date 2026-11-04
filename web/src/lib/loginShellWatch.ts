/**
 * 為登入新開的主機 shell（`openFreshShellAndType`）：畫面出現登入成功那行，就提示並自動收掉 pane
 * （使用者 2026-11-04：codex 裝置碼登入完成後 shell 還開著，要自己按「結束 shell」）。
 * 只記在記憶體、只看 (host, pane)；手動開的 shell 不在名單裡，不會被關。
 */
const watched = new Set<string>()

const keyOf = (host: string, paneId: string) => `${host || 'local'}/${paneId}`

export function watchLoginShell(host: string, paneId: string): void {
  watched.add(keyOf(host, paneId))
}

export function isLoginShell(host: string, paneId: string): boolean {
  return watched.has(keyOf(host, paneId))
}

export function forgetLoginShell(host: string, paneId: string): void {
  watched.delete(keyOf(host, paneId))
}

/** codex `Successfully logged in`、claude `Login successful`、其他 CLI 常見的 `Logged in as …`。 */
const SUCCESS_RE = /\b(successfully logged in|logged in successfully|login successful|logged in as\b)/i

export function loginSucceeded(text: string): boolean {
  return SUCCESS_RE.test(text)
}
