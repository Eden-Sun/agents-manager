import { ApiError } from '../api/types'

/**
 * daemon 的 `before=` 游標指不到東西（#766）：訊息已被刪掉（`before_message_gone`），或是別顆 bot／別個專案的
 * （`before_message_not_in_conversation`）。兩種的解法都一樣——重載第一頁拿新的邊界，不是重試同一個游標。
 */
export function isLostCursor(e: unknown): boolean {
  if (!(e instanceof ApiError) || e.status !== 404) return false
  const reason = e.body.reason
  return reason === 'before_message_gone' || reason === 'before_message_not_in_conversation'
}

/** 同一條時間軸一次只自救一次：重載後游標還是 404 就照舊報錯，不能無限迴圈。 */
const recovering = new Set<string>()

/**
 * 翻頁收到「游標丟了」時：重載第一頁、再翻一次（只一次）。`reload`／`again` 由呼叫端給；回 `true` 表示這次交給自救了
 * （呼叫端不必再報錯），`false` 表示不是這種錯、或這條時間軸正在自救（照舊報錯）。
 */
export async function recoverLostCursor(key: string, e: unknown, reload: () => Promise<void>, again: () => Promise<void>): Promise<boolean> {
  if (!isLostCursor(e) || recovering.has(key)) return false
  recovering.add(key)
  try {
    await reload()
    await again()
  } finally {
    recovering.delete(key)
  }
  return true
}
