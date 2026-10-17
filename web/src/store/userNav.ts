/**
 * 使用者導覽計數：點 bot、切分頁、開設定、看 shell 等選取動作都會加一。
 * 網址還原靠它分辨「使用者動作」與「還原／刷新寫進來的選取」（#1808）：後者不算導覽，不會作廢還沒回來的深連結。
 */
let count = 0

export function markUserNav(): void {
  count++
}

export function userNavCount(): number {
  return count
}
