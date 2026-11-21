/**
 * 承諾帳本的 REST 封裝（daemon `daemon/src/ledger_api.rs`）。使用者從網頁寫入（user principal）。
 * 版本衝突回 409（`ApiError.body.reason === 'ledger_version_conflict'`）：呼叫端要重新拉取再操作。
 */
import { rawTransport } from './index'
import { toLedgerItem } from './normalize'
import type { LedgerItem } from './types'

function itemsOf(raw: unknown): LedgerItem[] {
  const list = raw && typeof raw === 'object' && Array.isArray((raw as { items?: unknown }).items) ? (raw as { items: unknown[] }).items : []
  return list.map(toLedgerItem).filter((x): x is LedgerItem => x !== null)
}

/** 專案帳本（可限定某一則訊息）。 */
export async function fetchProjectLedger(projectId: string, opts: { sourceMessageId?: string } = {}): Promise<LedgerItem[]> {
  const q = opts.sourceMessageId ? `?source_message_id=${encodeURIComponent(opts.sourceMessageId)}` : ''
  return itemsOf(await rawTransport.request('GET', `/projects/${encodeURIComponent(projectId)}/ledger${q}`))
}

/** 新增後續承諾（補漏）；帶 `source_message_id` 就掛在那則訊息下面。 */
export async function createLedgerItems(
  projectId: string,
  body: { source_message_id?: string; title: string; status?: 'following' | 'clarifying' | 'answered' },
): Promise<LedgerItem[]> {
  return itemsOf(await rawTransport.request('POST', `/projects/${encodeURIComponent(projectId)}/ledger`, body))
}

/** 單項操作：`update`（改狀態／內容）、`merge`（併入另一項）、`cancel`。一律帶 `expected_version`。 */
export type LedgerPatch =
  | { op?: 'update'; expected_version: number; status?: string; title?: string }
  | { op: 'merge'; expected_version: number; merged_into_id: string }
  | { op: 'cancel'; expected_version: number; close_reason?: string }

export async function patchLedgerItem(projectId: string, itemId: string, body: LedgerPatch): Promise<LedgerItem | null> {
  const raw = await rawTransport.request('PATCH', `/projects/${encodeURIComponent(projectId)}/ledger/${encodeURIComponent(itemId)}`, body)
  return toLedgerItem(raw)
}
