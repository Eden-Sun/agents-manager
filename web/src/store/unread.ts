
import type { Message, Project } from '../api/types'
import { writeShared } from './mobilePreview'
import { laterMark } from './sharedUnread'

const MARKS_KEY = 'am.readMarks'
const COUNTS_KEY = 'am.unread'

export type UnreadKey = `bot:${string}`

export const botKey = (botId: string): UnreadKey => `bot:${botId}`

export interface ReadMark {
  at: string
  id: string
  seq?: number
}

function isRec(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

export function loadMarks(): Record<string, ReadMark> {
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(MARKS_KEY) ?? 'null')
    if (!isRec(parsed)) return {}
    const out: Record<string, ReadMark> = {}
    for (const [k, v] of Object.entries(parsed)) {
      if (!isRec(v)) continue
      const at = v.at
      const id = v.id
      if (typeof at !== 'string' || !at) continue
      const seq = v.seq
      out[k] = { at, id: typeof id === 'string' ? id : '', ...(typeof seq === 'number' && Number.isFinite(seq) ? { seq } : {}) }
    }
    return out
  } catch {
    return {}
  }
}

export function recordReadMark(current: Record<string, ReadMark>, key: string, mark: ReadMark): Record<string, ReadMark> {
  const merged = mergeWithDisk(current)
  merged[key] = laterMark(merged[key], mark) ?? mark
  saveMarks(merged)
  return merged
}

function mergeWithDisk(current: Record<string, ReadMark>): Record<string, ReadMark> {
  const merged: Record<string, ReadMark> = { ...current }
  for (const [k, v] of Object.entries(loadMarks())) merged[k] = laterMark(merged[k], v) ?? v
  return merged
}

export function prunePersistedMarks(current: Record<string, ReadMark>, liveBot: (id: string) => boolean): Record<string, ReadMark> {
  const pruned = pruneMarks(mergeWithDisk(current), liveBot)
  saveMarks(pruned)
  return pruned
}

export function saveMarks(marks: Record<string, ReadMark>) {
  try {
    writeShared(() => localStorage.setItem(MARKS_KEY, JSON.stringify(marks)))
  } catch {
  }
}

export interface UnreadCounts {
  bots: Record<string, number>
}

export function loadCounts(): UnreadCounts {
  const out: UnreadCounts = { bots: {} }
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(COUNTS_KEY) ?? 'null')
    if (!isRec(parsed)) return out
    for (const [k, v] of Object.entries(parsed)) {
      const n = typeof v === 'number' && Number.isFinite(v) ? Math.floor(v) : 0
      if (n <= 0) continue
      if (k.startsWith('bot:')) out.bots[k.slice(4)] = n
    }
  } catch {
  }
  return out
}

export function saveCounts(counts: UnreadCounts) {
  const flat: Record<string, number> = {}
  for (const [id, n] of Object.entries(counts.bots)) if (n > 0) flat[botKey(id)] = n
  try {
    writeShared(() => localStorage.setItem(COUNTS_KEY, JSON.stringify(flat)))
  } catch {
  }
}

export function windowActive(): boolean {
  if (typeof document === 'undefined') return true
  if (document.visibilityState !== 'visible') return false
  return typeof document.hasFocus === 'function' ? document.hasFocus() : true
}

export function completesTurn(msg: Pick<Message, 'role' | 'keep_warm'>): boolean {
  return msg.role === 'assistant' && !msg.keep_warm
}

export function isUnread(msg: Pick<Message, 'id' | 'created_at' | 'seq'>, mark: ReadMark | undefined): boolean {
  if (!mark) return true
  if (msg.created_at > mark.at) return true
  if (msg.created_at !== mark.at) return false
  if (msg.seq != null && mark.seq != null) return msg.seq > mark.seq
  return msg.id > mark.id
}

export function relayTurnIds(messages: readonly Message[]): Set<string> {
  const human = new Set<string>()
  const relay = new Set<string>()
  for (const m of messages) {
    if (m.role !== 'user' || !m.turn_id) continue
    if (m.relay_from == null) human.add(m.turn_id)
    else relay.add(m.turn_id)
  }
  for (const id of human) relay.delete(id)
  return relay
}

export function isExpertBot(bot: { project_id: string; role?: string | null } | undefined, projects: readonly Pick<Project, 'id' | 'mode'>[]): boolean {
  if (!bot || bot.role === 'manager') return false
  return projects.some((p) => p.id === bot.project_id && p.mode === 'expert')
}

export function countUnreadTurns(messages: readonly Message[], mark: ReadMark | undefined, expert = false): number {
  const relay = expert ? relayTurnIds(messages) : null
  const turns = new Set<string>()
  for (const m of messages) {
    if (!completesTurn(m) || !isUnread(m, mark)) continue
    if (m.turn_id && relay?.has(m.turn_id)) continue
    turns.add(m.turn_id ?? `msg:${m.id}`)
  }
  return turns.size
}

export function completionKey(msg: Pick<Message, 'id' | 'turn_id'>, knownTurnIds: readonly string[]): string {
  if (msg.turn_id) return msg.turn_id
  let latest: string | null = null
  for (const id of knownTurnIds) if (!latest || id > latest) latest = id
  return latest ?? `msg:${msg.id}`
}

export function markOfMessages(messages: readonly Message[]): ReadMark | null {
  let best: ReadMark | null = null
  for (const m of messages) {
    const laterAt = !best || m.created_at > best.at
    const laterTie = best && m.created_at === best.at && (
      m.seq != null && best.seq != null ? m.seq > best.seq : m.id > best.id
    )
    if (laterAt || laterTie) best = { at: m.created_at, id: m.id, ...(m.seq == null ? {} : { seq: m.seq }) }
  }
  return best
}

export function markNow(): ReadMark {
  return { at: new Date().toISOString(), id: '' }
}

export function pruneUnread<T>(book: Record<string, T>, live: (id: string) => boolean): Record<string, T> {
  const out: Record<string, T> = {}
  for (const [k, v] of Object.entries(book)) if (live(k)) out[k] = v
  return out
}

export function pruneMarks(marks: Record<string, ReadMark>, liveBot: (id: string) => boolean) {
  return pruneUnread(marks, (k) => k.startsWith('bot:') && liveBot(k.slice(4)))
}

export function unreadShown(bot: { project_id: string } | undefined, supervisorProjectId: string | null): boolean {
  return !(bot && supervisorProjectId && bot.project_id === supervisorProjectId)
}

export function unreadCounted(
  bot: { id: string; project_id: string },
  supervisorProjectId: string | null,
  hidden: ReadonlySet<string>,
): boolean {
  return unreadShown(bot, supervisorProjectId) && !hidden.has(bot.id)
}

export interface UnreadBook {
  botUnread: Record<string, number>
  hiddenBotIds: readonly string[]
  bots: readonly { id: string; project_id: string }[]
  supervisorProjectId: string | null
}

function sumUnread(s: UnreadBook, keep: (bot: { id: string; project_id: string }) => boolean): number {
  const hidden = new Set(s.hiddenBotIds)
  let n = 0
  for (const b of s.bots) {
    if (!unreadCounted(b, s.supervisorProjectId, hidden) || !keep(b)) continue
    n += s.botUnread[b.id] ?? 0
  }
  return n
}

export function titleUnread(s: UnreadBook): number {
  return sumUnread(s, () => true)
}

export function projectUnread(s: UnreadBook, projectId: string): number {
  return sumUnread(s, (b) => b.project_id === projectId)
}

const counted = new Set<string>()
const COUNTED_CAP = 500

export function takeTurnCompletion(botId: string, turnId: string): boolean {
  const key = `${botId}:${turnId}`
  if (counted.has(key)) return false
  if (counted.size >= COUNTED_CAP) {
    const oldest = counted.values().next().value
    if (oldest !== undefined) counted.delete(oldest)
  }
  counted.add(key)
  return true
}

export function resetTurnCompletions() {
  counted.clear()
}

const hookCompleted = new Set<string>()
const idleEdges = new Map<string, number>()

export function markHookCompletion(botId: string) {
  hookCompleted.add(botId)
}

export function clearHookCompletion(botId: string) {
  hookCompleted.delete(botId)
}

export function idleEdgeCompletionKey(
  botId: string,
  runId: string,
  latestTurn: { id: string; status: string } | null,
): string | null {
  if (latestTurn && latestTurn.status === 'in_flight') return latestTurn.id
  if (hookCompleted.delete(botId)) return null
  const n = (idleEdges.get(botId) ?? 0) + 1
  idleEdges.set(botId, n)
  return `run:${runId}:${n}`
}

export function pruneIdleEdges(live: ReadonlySet<string>) {
  for (const id of [...hookCompleted]) if (!live.has(id)) hookCompleted.delete(id)
  for (const id of [...idleEdges.keys()]) if (!live.has(id)) idleEdges.delete(id)
}

export function resetIdleEdges() {
  hookCompleted.clear()
  idleEdges.clear()
}
