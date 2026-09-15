import type { Message, Turn } from '../api/types'
import { loadCounts, saveCounts, type UnreadCounts } from './unread'

/**
 * 專案標題的藍色數字只算「群組回覆」（2026-09-15 使用者選 1）：以前專案裡任何 bot 完成回合都 +1，
 * Agents Manager 這種 bot 多的專案一下就 99+，跟說明「未讀的群組回覆」對不上。
 *
 * 群組回覆＝回的是群組訊息的那一回合。唯一明確的證據是那顆 bot 對話裡同 `turn_id`、帶 `group_id` 的
 * user 訊息（API.md §11.1；直接 prompt API 設不了 `group_id`）。turn 本身沒有群組欄位。
 * - WS `message_added` 與載入的訊息頁（群組時間軸、單 bot 對話）看過就記下。
 * - 沒看過的（重整前送出、新分頁、沒打開的專案）：群組 fan-out 的 `client_request_id` 是 `<group_id>:<bot_id>`
 *   （§11.2），只拿它篩候選，再抓那顆 bot 的訊息頁確認 `group_id`，不把命名慣例當證據。
 */
const groupTurns = new Set<string>()
/** 確認過不是群組回合的（候選但訊息頁沒有 `group_id`），避免同一回合的訊息與 turn frame 各抓一次。 */
const directTurns = new Set<string>()
const confirming = new Map<string, Promise<boolean>>()
const MAX_TRACKED = 500

function remember(set: Set<string>, id: string) {
  set.add(id)
  // Set 依插入順序：超量就丟最舊的。
  while (set.size > MAX_TRACKED) {
    const oldest = set.values().next().value
    if (oldest === undefined) break
    set.delete(oldest)
  }
}

export function noteGroupPrompt(msg: Pick<Message, 'role' | 'group_id' | 'turn_id'>) {
  if (msg.role !== 'user' || !msg.group_id || !msg.turn_id) return
  remember(groupTurns, msg.turn_id)
}

/** 載入的群組時間軸、單 bot 對話頁都是來源。 */
export function noteGroupPrompts(messages: readonly Pick<Message, 'role' | 'group_id' | 'turn_id'>[]) {
  for (const m of messages) noteGroupPrompt(m)
}

export function isGroupTurn(turnId: string | null | undefined): boolean {
  return Boolean(turnId && groupTurns.has(turnId))
}

/** 候選：`client_request_id` 以收件 bot 自己的 id 結尾。只用來決定要不要抓訊息確認。 */
export function isGroupTurnCandidate(turn: Pick<Turn, 'client_request_id'> | null | undefined, botId: string): boolean {
  const crid = turn?.client_request_id
  const suffix = `:${botId}`
  return Boolean(botId && crid && crid.length > suffix.length && crid.endsWith(suffix))
}

type PageMessage = Pick<Message, 'id' | 'role' | 'group_id' | 'turn_id' | 'created_at'>
/** `GET /api/bots/:id/messages?limit=&before=`：最新（或 `before` 之前）一頁，頁內舊到新（API.md §6）。 */
export type LoadMessagesPage = (botId: string, limit: number, before?: string) => Promise<{ messages: readonly PageMessage[]; has_more: boolean }>

const PAGE = 50
const MAX_PAGES = 4
const RETRY_MS = [1_000, 4_000]

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

/** 往前翻到那回合開始之前（該回合的 user 訊息一定在那之後）；最多 MAX_PAGES 頁，有界。 */
async function scanForGroupPrompt(botId: string, turnId: string, since: string, load: LoadMessagesPage): Promise<boolean> {
  // 用時間值比（秒與毫秒兩種寫法字串比會錯）；留一分鐘餘裕，user 訊息跟 turn 不是同一刻寫入。
  const sinceMs = Date.parse(since) - 60_000
  let before: string | undefined
  for (let i = 0; i < MAX_PAGES; i++) {
    const page = await load(botId, PAGE, before)
    noteGroupPrompts(page.messages)
    if (groupTurns.has(turnId)) return true
    const oldest = page.messages[0]
    if (!page.has_more || !oldest || Date.parse(oldest.created_at) < sinceMs) return false
    before = oldest.id
  }
  return false
}

/**
 * 認得就回 true；候選就抓訊息頁找同回合帶 `group_id` 的 user 訊息。同一回合同時只抓一次。
 * 確定不是才記進 `directTurns`；抓失敗退避重試，全失敗也不記，之後的 frame 還能再試。
 */
export async function confirmGroupTurn(
  botId: string,
  turnId: string | null | undefined,
  turn: Pick<Turn, 'client_request_id' | 'created_at'> | null | undefined,
  load: LoadMessagesPage,
  retryMs: readonly number[] = RETRY_MS,
): Promise<boolean> {
  if (!turnId) return false
  if (groupTurns.has(turnId)) return true
  if (directTurns.has(turnId) || !isGroupTurnCandidate(turn, botId)) return false
  const inflight = confirming.get(turnId)
  if (inflight) return inflight
  const p = (async () => {
    try {
      for (let attempt = 0; ; attempt++) {
        try {
          const yes = await scanForGroupPrompt(botId, turnId, turn?.created_at ?? '', load)
          if (!yes) remember(directTurns, turnId)
          return yes
        } catch {
          if (attempt >= retryMs.length) return false
          await sleep(retryMs[attempt])
        }
      }
    } finally {
      confirming.delete(turnId)
    }
  })()
  confirming.set(turnId, p)
  return p
}

/**
 * v2 是 72c332a 的標記：它只寫標記、沒把清空存回去，已部署的分頁可能帶著 v2 又讀回 99+。
 * 分不出 v2 之後的群組數字哪些是舊的，所以換 v3 再清一次（最多丟掉幾個小時內的群組未讀）。
 */
const RESET_KEY = 'am.groupUnread.v3'

/**
 * 舊算法累積的群組數字（常是 99+）升級後清一次，之後照新規則算；bot 未讀原樣保留。
 *
 * 先把清空的結果寫回 `am.unread` 並讀回確認，才寫遷移標記：只寫標記不存檔的話，下次重整前沒有別的
 * 未讀變動就會把舊的 99+ 讀回來。存檔失敗就不寫標記，下次開機再清一次。
 */
export function dropLegacyGroupCounts(counts: UnreadCounts): UnreadCounts {
  const cleared: UnreadCounts = { bots: counts.bots, groups: {} }
  try {
    if (localStorage.getItem(RESET_KEY)) return counts
  } catch {
    // 讀不到 storage：`loadCounts` 也讀不到，本來就沒有舊數字。
    return cleared
  }
  if (Object.keys(counts.groups).length > 0) {
    saveCounts(cleared)
    if (Object.keys(loadCounts().groups).length > 0) return cleared
  }
  try {
    localStorage.setItem(RESET_KEY, '1')
  } catch {
    /* 標記寫不進去：數字已清，下次開機再清一次也無害 */
  }
  return cleared
}

export function resetGroupTurnsForTest() {
  groupTurns.clear()
  directTurns.clear()
  confirming.clear()
}
