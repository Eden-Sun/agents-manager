/** 主機 shell 的「縮放至全螢幕」偏好：每個瀏覽器各記一份（localStorage），預設關。 */
const KEY = 'am.shellFitScreen'

export function readFitScreen(): boolean {
  try {
    return localStorage.getItem(KEY) === '1'
  } catch {
    // 無痕模式讀不到：當作沒設過（關）。
    return false
  }
}

export function writeFitScreen(on: boolean): void {
  try {
    localStorage.setItem(KEY, on ? '1' : '0')
  } catch {
    /* 記不起來就只在這一頁有效 */
  }
}
