// 刪 AGM 的 bot 要第二次確認（issue #406）：daemon 對「總管／角色本身、它們的 child、總管專案裡的工人」的
// `DELETE /api/bots/{id}` 回 409 `supervisor_owned`，要帶 `?confirm=supervisor` 才刪。網頁不能在這裡變死路——
// 13:28Z 那兩刀很可能就是從網頁按的——所以認出這個 409 就改問一次，寫清楚是 AGM 的哪一顆。
import { ApiError } from '../api/types'

export interface AgmDeleteAsk {
  botId: string
  name: string
  /** daemon 給的角色名（`AGM 協調者`、`AGM 專案裡的常駐工人`…）；舊 daemon 沒給就是 null。 */
  role: string | null
}

/** 這個錯是不是「要刪的是 AGM 的 bot、需要明講」。是的話回要問使用者的內容，否則 null。 */
export function supervisorOwnedAsk(e: unknown, botId: string, fallbackName: string): AgmDeleteAsk | null {
  if (!(e instanceof ApiError) || e.status !== 409 || e.body.reason !== 'supervisor_owned') return null
  const body = e.body as Record<string, unknown>
  const name = typeof body.name === 'string' && body.name.trim() ? body.name : fallbackName
  const role = typeof body.role === 'string' && body.role.trim() ? body.role : null
  return { botId, name, role }
}
