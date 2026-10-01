/**
 * 長時間開著的分頁：store 裡以 bot／專案／任務 id 為鍵的表（訊息、載入旗標、草稿、預覽、任務細節…）
 * 以前只加不減——bot 被刪（子 agent 退役是常態）、專案被刪之後，它們的訊息陣列與旗標一直留在記憶體裡。
 * 快照（`GET /api/state`）才是「誰還在」的權威，所以每次拿到快照就把不在名單上的 key 帶走
 * （`pruneUnread`／`pruneMarks` 是同一個做法，只管未讀帳）。
 *
 * 純函式：回傳只含**有變**的欄位，什麼都沒掉就回 `null`（呼叫端不必 `set`、也不會多一次 render）。
 * 軟刪除的 bot 復原之後會重新出現：它的訊息本來就是選到它時才載（`loadedBots` 被帶走 → 會重載），不會丟資料。
 */
import type { Bot, Project } from '../api/types'
import type { StoreState } from './store'

type Rec<V> = Record<string, V>

export type PrunableState = Pick<
  StoreState,
  | 'bots'
  | 'projects'
  | 'messages'
  | 'loadedBots'
  | 'moreMessages'
  | 'loadingMore'
  | 'messageCapFloors'
  | 'liveReply'
  | 'composerDrafts'
  | 'previews'
  | 'groupMessages'
  | 'loadedProjects'
  | 'missions'
  | 'missionsCapped'
  | 'missionDetail'
  | 'missionLoading'
  | 'missionLoadErrors'
  | 'sidePanes'
  | 'botOrder'
  | 'drafts'
  | 'draftCursors'
>

/** 只留 `keep(key)` 為真的；一個都沒掉就回原物件（同一個參考）。 */
function keepKeys<V>(rec: Rec<V>, keep: (key: string) => boolean): Rec<V> {
  let out: Rec<V> | null = null
  for (const key of Object.keys(rec)) {
    if (keep(key)) continue
    out ??= { ...rec }
    delete out[key]
  }
  return out ?? rec
}

/** 草稿的 key：`bot:<id>`／`group:<id>`；別種（`shell:…`）不是這裡管的，一律留著。 */
function draftAlive(key: string, liveBot: (id: string) => boolean, liveProject: (id: string) => boolean): boolean {
  if (key.startsWith('bot:')) return liveBot(key.slice(4))
  if (key.startsWith('group:')) return liveProject(key.slice(6))
  return true
}

export function pruneDeadKeys(s: PrunableState): Partial<PrunableState> | null {
  const botIds = new Set(s.bots.map((b: Bot) => b.id))
  const projectIds = new Set(s.projects.map((p: Project) => p.id))
  const liveBot = (id: string) => botIds.has(id)
  const liveProject = (id: string) => projectIds.has(id)
  // `moreMessages`／`loadingMore`／`messageCapFloors` 的 key 可能是 bot id 也可能是專案 id（群組時間軸）。
  const liveConversation = (id: string) => botIds.has(id) || projectIds.has(id)
  // 任務細節只為清單上的卡片載入；不在任何清單上（結案超過上限、專案沒了）的沒有人會再看。
  const missionsByProject = keepKeys(s.missions, liveProject)
  const missionIds = new Set<string>()
  for (const list of Object.values(missionsByProject)) for (const m of list) missionIds.add(m.id)
  const liveMission = (id: string) => missionIds.has(id)

  const next: Partial<PrunableState> = {}
  const set = <K extends keyof PrunableState>(key: K, value: PrunableState[K]) => {
    if (value !== s[key]) next[key] = value
  }
  set('messages', keepKeys(s.messages, liveBot))
  set('loadedBots', keepKeys(s.loadedBots, liveBot))
  set('moreMessages', keepKeys(s.moreMessages, liveConversation))
  set('loadingMore', keepKeys(s.loadingMore, liveConversation))
  set('messageCapFloors', keepKeys(s.messageCapFloors, liveConversation))
  set('liveReply', keepKeys(s.liveReply, liveBot))
  set('composerDrafts', keepKeys(s.composerDrafts, liveBot))
  set('previews', keepKeys(s.previews, liveBot))
  set('groupMessages', keepKeys(s.groupMessages, liveProject))
  set('loadedProjects', keepKeys(s.loadedProjects, liveProject))
  set('missions', missionsByProject)
  set('missionsCapped', keepKeys(s.missionsCapped, liveProject))
  set('missionDetail', keepKeys(s.missionDetail, liveMission))
  set('missionLoading', keepKeys(s.missionLoading, liveMission))
  set('missionLoadErrors', keepKeys(s.missionLoadErrors, liveMission))
  set('sidePanes', keepKeys(s.sidePanes, liveProject))
  set('botOrder', keepKeys(s.botOrder, liveProject))
  set('drafts', keepKeys(s.drafts, (k) => draftAlive(k, liveBot, liveProject)))
  set('draftCursors', keepKeys(s.draftCursors, (k) => draftAlive(k, liveBot, liveProject)))
  return Object.keys(next).length > 0 ? next : null
}
