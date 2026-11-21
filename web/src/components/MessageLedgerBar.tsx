import { useEffect, useState } from 'react'
import { createLedgerItems, fetchProjectLedger, patchLedgerItem } from '../api/ledger'
import type { LedgerItem, LedgerItemStatus, Message } from '../api/types'
import { ledgerSummary } from '../lib/ledgerSummary'
import { ApiError } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import './messageLedgerBar.css'

/** 每個狀態的顯示順序與字；`merged` 併入別項，不另列。 */
const STATUS_ORDER: LedgerItemStatus[] = ['untriaged', 'answered', 'following', 'clarifying', 'done', 'cancelled']

/**
 * 使用者訊息旁的承諾帳本進度列（SPEC 帳本第 1 階段 T5）：一訊多事時逐項顯示狀態，點開可補漏、合併、改狀態或取消。
 * 初始資料來自 `msg.ledger_items`（daemon 附掛）；`ledger_updated` 事件到達時重新拉這一則的項目。
 */
export function MessageLedgerBar({ msg }: { msg: Message }) {
  const t = useT()
  const projectId = useStore((s) => s.bots.find((b) => b.id === msg.bot_id)?.project_id ?? null)
  const bots = useStore((s) => s.bots)
  const botName = (id: string | null) => (id ? bots.find((b) => b.id === id)?.name ?? id : '')
  const rev = useStore((s) => s.ledgerRev[msg.id] ?? 0)
  const notify = useStore((s) => s.notify)
  const [items, setItems] = useState<LedgerItem[] | null>(msg.ledger_items ?? null)
  const [open, setOpen] = useState(false)
  const [mode, setMode] = useState<null | { kind: 'add' | 'merge'; itemId?: string }>(null)
  const [draft, setDraft] = useState('')
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    if (msg.ledger_items) setItems(msg.ledger_items)
  }, [msg.ledger_items])

  // 沒有附掛（daemon 只附有項目的訊息）就不發請求；只有 `ledger_updated` 到達（rev 變動）才重新拉這一則。
  useEffect(() => {
    if (!projectId || rev === 0) return
    let alive = true
    void fetchProjectLedger(projectId, { sourceMessageId: msg.id })
      .then((list) => alive && setItems(list))
      .catch(() => {})
    return () => {
      alive = false
    }
  }, [projectId, msg.id, rev])

  if (!projectId || !items || items.length === 0) return null

  const visible = items.filter((it) => it.status !== 'merged')
  const counts = ledgerSummary(items)
  const untriagedOnly = visible.length === 1 && visible[0].status === 'untriaged'
  const parts = STATUS_ORDER.filter((s) => (counts[s] ?? 0) > 0).map((s) => `${statusLabel(s, t)} ${counts[s]}`)

  const applyItem = (next: LedgerItem) => setItems((cur) => (cur ?? []).map((it) => (it.id === next.id ? next : it)))
  const runPatch = async (fn: () => Promise<LedgerItem | null>) => {
    setBusy(true)
    try {
      const next = await fn()
      if (next) applyItem(next)
      setMode(null)
      setDraft('')
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        notify('error', t('這一項已被別人更新，已重新載入，請再操作一次'))
        const list = await fetchProjectLedger(projectId, { sourceMessageId: msg.id }).catch(() => null)
        if (list) setItems(list)
      } else {
        notify('error', t('帳本操作失敗'))
      }
    } finally {
      setBusy(false)
    }
  }

  const addItem = () =>
    runPatch(async () => {
      const title = draft.trim()
      if (!title) return null
      const created = await createLedgerItems(projectId, { source_message_id: msg.id, title, status: 'following' })
      setItems((cur) => [...(cur ?? []), ...created])
      return null
    })

  const mergeInto = (item: LedgerItem) =>
    runPatch(async () => {
      const target = draft.trim().toUpperCase()
      const hit = items.find((it) => it.code.toUpperCase() === target && it.id !== item.id)
      if (!hit) {
        notify('error', t('找不到這則訊息底下的 {code}', { code: target || '—' }))
        return null
      }
      return patchLedgerItem(projectId, item.id, { op: 'merge', expected_version: item.version, merged_into_id: hit.id })
    })

  const setStatus = (item: LedgerItem, status: 'done' | 'following') =>
    runPatch(() => patchLedgerItem(projectId, item.id, { op: 'update', expected_version: item.version, status }))
  const cancelItem = (item: LedgerItem) =>
    runPatch(() => patchLedgerItem(projectId, item.id, { op: 'cancel', expected_version: item.version, close_reason: 'cancelled by user' }))

  return (
    <div className="ledger-bar" aria-label={t('承諾帳本進度')}>
      <button type="button" className="ledger-bar-summary" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <span aria-hidden="true">📋</span>{' '}
        {untriagedOnly ? `${t('待分流')} 1` : t('{n} 件：{parts}', { n: visible.length, parts: parts.join('／') })}
      </button>
      {open ? (
        <ul className="ledger-list">
          {visible.map((it) => (
            <li key={it.id} className={`ledger-item status-${it.status}`}>
              <div className="ledger-item-head">
                <span className="ledger-code">{it.code}</span>
                <span className={`ledger-status status-${it.status}`}>{statusLabel(it.status, t)}</span>
                <span className="ledger-title">{it.title}</span>
              </div>
              <div className="ledger-item-meta">
                {it.owner_bot_id ? <span>{t('承辦')}：{botName(it.owner_bot_id)}</span> : null}
                {it.next_step ? <span>{t('下一步')}：{it.next_step}</span> : null}
              </div>
              <div className="ledger-actions">
                <button type="button" className="mini-btn" disabled={busy} onClick={() => setMode({ kind: 'add' })}>
                  {t('＋補漏')}
                </button>
                <button type="button" className="mini-btn" disabled={busy} onClick={() => setMode({ kind: 'merge', itemId: it.id })}>
                  {t('合併')}
                </button>
                {it.status !== 'done' ? (
                  <button type="button" className="mini-btn" disabled={busy} onClick={() => void setStatus(it, 'done')}>
                    {t('標為完成')}
                  </button>
                ) : null}
                {it.status === 'done' ? (
                  <button type="button" className="mini-btn" disabled={busy} onClick={() => void setStatus(it, 'following')}>
                    {t('改為待跟進')}
                  </button>
                ) : null}
                <button type="button" className="mini-btn danger" disabled={busy} onClick={() => void cancelItem(it)}>
                  {t('取消')}
                </button>
              </div>
              {mode && mode.itemId === it.id && mode.kind === 'merge' ? (
                <div className="ledger-inline">
                  <input value={draft} onChange={(e) => setDraft(e.target.value)} placeholder={t('併入哪一項（如 L12）')} aria-label={t('併入哪一項')} />
                  <button type="button" className="mini-btn" disabled={busy || !draft.trim()} onClick={() => void mergeInto(it)}>
                    {t('確定合併')}
                  </button>
                </div>
              ) : null}
            </li>
          ))}
          {mode && mode.kind === 'add' ? (
            <li className="ledger-inline">
              <input value={draft} onChange={(e) => setDraft(e.target.value)} placeholder={t('補上漏掉的一件事')} aria-label={t('補漏的內容')} />
              <button type="button" className="mini-btn" disabled={busy || !draft.trim()} onClick={() => void addItem()}>
                {t('新增')}
              </button>
            </li>
          ) : null}
        </ul>
      ) : null}
    </div>
  )
}

function statusLabel(s: LedgerItemStatus, t: ReturnType<typeof useT>): string {
  switch (s) {
    case 'untriaged':
      return t('待分流')
    case 'answered':
      return t('已答')
    case 'following':
      return t('待跟進')
    case 'clarifying':
      return t('待釐清')
    case 'done':
      return t('已完成')
    case 'cancelled':
      return t('已取消')
    case 'merged':
      return t('已併入')
  }
}
