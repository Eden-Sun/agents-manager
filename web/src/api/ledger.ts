import { rawTransport } from './index'
import { toLedgerItem } from './normalize'
import type { LedgerItem } from './types'

function itemsOf(raw: unknown): LedgerItem[] {
  const list = raw && typeof raw === 'object' && Array.isArray((raw as { items?: unknown }).items) ? (raw as { items: unknown[] }).items : []
  return list.map(toLedgerItem).filter((x): x is LedgerItem => x !== null)
}

export async function fetchProjectLedger(projectId: string, opts: { sourceMessageId?: string; status?: 'open' } = {}): Promise<LedgerItem[]> {
  const params = new URLSearchParams()
  if (opts.sourceMessageId) params.set('source_message_id', opts.sourceMessageId)
  if (opts.status) params.set('status', opts.status)
  const q = params.toString() ? `?${params.toString()}` : ''
  return itemsOf(await rawTransport.request('GET', `/projects/${encodeURIComponent(projectId)}/ledger${q}`))
}

export async function createLedgerItems(
  projectId: string,
  body: { source_message_id?: string; title: string; status?: 'untriaged' | 'following' | 'clarifying' | 'answered' },
): Promise<LedgerItem[]> {
  return itemsOf(await rawTransport.request('POST', `/projects/${encodeURIComponent(projectId)}/ledger`, body))
}

export type LedgerPatch =
  | { op?: 'update'; expected_version: number; status?: string; title?: string; next_step?: string; wait_for?: string; due_at?: string }
  | { op: 'merge'; expected_version: number; merged_into_id: string }
  | { op: 'cancel'; expected_version: number; close_reason?: string }

export async function patchLedgerItem(projectId: string, itemId: string, body: LedgerPatch): Promise<LedgerItem | null> {
  const raw = await rawTransport.request('PATCH', `/projects/${encodeURIComponent(projectId)}/ledger/${encodeURIComponent(itemId)}`, body)
  return toLedgerItem(raw)
}

export async function completeLedgerItem(projectId: string, item: LedgerItem): Promise<LedgerItem | null> {
  return patchLedgerItem(projectId, item.id, { op: 'update', expected_version: item.version, status: 'done' })
}

export async function notSureLedgerItem(projectId: string, item: LedgerItem, nextStep: string): Promise<LedgerItem | null> {
  return patchLedgerItem(projectId, item.id, {
    op: 'update',
    expected_version: item.version,
    status: 'following',
    next_step: nextStep,
    wait_for: '',
    due_at: new Date().toISOString(),
  })
}
