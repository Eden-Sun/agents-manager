import type { Bot, Project } from '../api/types'
import type { StoreState } from './store'

type Rec<V> = Record<string, V>

export type PrunableState = Pick<
  StoreState,
  | 'bots'
  | 'projects'
  | 'messages'
  | 'turns'
  | 'messageLoadFailed'
  | 'shareRev'
  | 'loadedBots'
  | 'moreMessages'
  | 'loadingMore'
  | 'messageCapFloors'
  | 'liveReply'
  | 'composerDrafts'
  | 'previews'
  | 'sidePanes'
  | 'botOrder'
  | 'drafts'
  | 'draftCursors'
>

function keepKeys<V>(rec: Rec<V>, keep: (key: string) => boolean): Rec<V> {
  let out: Rec<V> | null = null
  for (const key of Object.keys(rec)) {
    if (keep(key)) continue
    out ??= { ...rec }
    delete out[key]
  }
  return out ?? rec
}

function draftAlive(key: string, liveBot: (id: string) => boolean, _liveProject: (id: string) => boolean): boolean {
  if (key.startsWith('bot:')) return liveBot(key.slice(4))
  if (key.startsWith('group:')) return false
  return true
}

export function pruneDeadKeys(s: PrunableState): Partial<PrunableState> | null {
  const botIds = new Set(s.bots.map((b: Bot) => b.id))
  const projectIds = new Set(s.projects.map((p: Project) => p.id))
  const liveBot = (id: string) => botIds.has(id)
  const liveProject = (id: string) => projectIds.has(id)
  const liveConversation = liveBot

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
  set('turns', keepKeys(s.turns, liveBot))
  set('messageLoadFailed', keepKeys(s.messageLoadFailed, liveBot))
  set('shareRev', keepKeys(s.shareRev, liveBot))
  set('composerDrafts', keepKeys(s.composerDrafts, liveBot))
  set('previews', keepKeys(s.previews, liveBot))
  set('sidePanes', keepKeys(s.sidePanes, liveProject))
  set('botOrder', keepKeys(s.botOrder, liveProject))
  set('drafts', keepKeys(s.drafts, (k) => draftAlive(k, liveBot, liveProject)))
  set('draftCursors', keepKeys(s.draftCursors, (k) => draftAlive(k, liveBot, liveProject)))
  return Object.keys(next).length > 0 ? next : null
}
