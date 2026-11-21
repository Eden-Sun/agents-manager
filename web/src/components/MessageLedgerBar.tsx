import { useEffect, useState } from 'react'
import { createLedgerItems, fetchProjectLedger, patchLedgerItem } from '../api/ledger'
import type { LedgerItem, LedgerItemStatus, Message } from '../api/types'
import { ApiError } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import './messageLedgerBar.css'

/** 使用者看到的白話狀態：不講「待跟進／已答」這類帳本術語。`merged`、`cancelled` 不列在摘要裡。 */
const SUMMARY_ORDER: LedgerItemStatus[] = ['untriaged', 'following', 'clarifying', 'answered', 'done']
/** 還可以按「⋯」操作的狀態（已完成、已回答的項目不再有操作）。 */
const ACTIONABLE: LedgerItemStatus[] = ['untriaged', 'following', 'clarifying']

/**
 * 使用者訊息下方一行的追蹤進度（SPEC 帳本第 1 階段 T5，2026-11-21 重做）：白話摘要；點開面板看這則訊息的追蹤任務。
 * 操作只在面板裡：每項「⋯」選「不用做了」或「已經好了」；面板底部可補一項。合併由管理者用 `agm ledger` 處理。
 * 初始資料來自 `msg.ledger_items`（daemon 附掛）；`ledger_updated` 事件到達（rev 變動）時重新拉這一則的項目。
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
  const [menuFor, setMenuFor] = useState<string | null>(null)
  const [adding, setAdding] = useState(false)
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

  // 要給使用者看的項目：排除併入別項的，以及已取消的（「不用做了」的不再計、也不列）。
  const live = items.filter((it) => it.status !== 'merged' && it.status !== 'cancelled')
  if (live.length === 0) return null
  const allUntriaged = live.every((it) => it.status === 'untriaged')
  const allClosed = live.every((it) => it.status === 'done' || it.status === 'answered')

  const counts: Partial<Record<LedgerItemStatus, number>> = {}
  for (const it of live) counts[it.status] = (counts[it.status] ?? 0) + 1
  const parts = SUMMARY_ORDER.filter((s) => (counts[s] ?? 0) > 0).map((s) => `${statusLabel(s, t)} ${counts[s]}`)

  const applyItem = (next: LedgerItem) => setItems((cur) => (cur ?? []).map((it) => (it.id === next.id ? next : it)))
  const runPatch = async (fn: () => Promise<LedgerItem | null>) => {
    setBusy(true)
    try {
      const next = await fn()
      if (next) applyItem(next)
      setMenuFor(null)
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

  const markDone = (item: LedgerItem) => runPatch(() => patchLedgerItem(projectId, item.id, { op: 'update', expected_version: item.version, status: 'done' }))
  const dismiss = (item: LedgerItem) =>
    runPatch(() => patchLedgerItem(projectId, item.id, { op: 'cancel', expected_version: item.version, close_reason: 'cancelled by user' }))
  // 「這則不用追蹤」：尚未整理時，把這則底下所有待整理的項目一起取消。
  const dismissAll = () =>
    runPatch(async () => {
      for (const it of live) await patchLedgerItem(projectId, it.id, { op: 'cancel', expected_version: it.version, close_reason: 'cancelled by user' })
      return null
    })

  const addItem = () =>
    runPatch(async () => {
      const title = draft.trim()
      if (!title) return null
      const created = await createLedgerItems(projectId, { source_message_id: msg.id, title, status: 'following' })
      setItems((cur) => [...(cur ?? []), ...created])
      setDraft('')
      setAdding(false)
      return null
    })

  const summary = allUntriaged
    ? t('已加入追蹤，等管理者整理')
    : allClosed
      ? t('追蹤完成')
      : t('追蹤 {n} 項：{parts}', { n: live.length, parts: parts.join('・') })

  return (
    <div className="ledger-bar" aria-label={t('承諾帳本進度')}>
      <button type="button" className="ledger-bar-summary" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <span aria-hidden="true">{allClosed ? '✓' : '📋'}</span> {summary}
      </button>
      {open ? (
        <>
          <div className="ledger-backdrop" aria-hidden="true" onClick={() => setOpen(false)} />
          <div className="ledger-panel" role="dialog" aria-label={t('這則訊息的追蹤任務')} onKeyDown={(e) => e.key === 'Escape' && setOpen(false)}>
            <div className="ledger-panel-head">
              <strong>{t('這則訊息的追蹤任務')}</strong>
              <button type="button" className="ledger-close" aria-label={t('關閉')} onClick={() => setOpen(false)}>
                ×
              </button>
            </div>

            {allUntriaged ? (
              <p className="ledger-hint">{t('管理者還沒整理，整理後會拆成具體任務')}</p>
            ) : (
              <ul className="ledger-rows">
                {live.map((it) => (
                  <li key={it.id} className={`ledger-row status-${it.status}`}>
                    <div className="ledger-row-main">
                      <span className="ledger-row-title">{it.title}</span>
                      <span className={`ledger-chip status-${it.status}`}>{statusLabel(it.status, t)}</span>
                      {ACTIONABLE.includes(it.status) ? (
                        <button
                          type="button"
                          className="ledger-more"
                          aria-label={t('更多操作')}
                          aria-haspopup="menu"
                          aria-expanded={menuFor === it.id}
                          disabled={busy}
                          onClick={() => setMenuFor((cur) => (cur === it.id ? null : it.id))}
                        >
                          ⋯
                        </button>
                      ) : null}
                    </div>
                    {it.next_step ? <div className="ledger-row-note">{t('下一步')}：{it.next_step}</div> : null}
                    {it.owner_bot_id ? <div className="ledger-row-owner">{t('承辦')}：{botName(it.owner_bot_id)}</div> : null}
                    {menuFor === it.id ? (
                      <div className="ledger-menu" role="menu">
                        <button type="button" role="menuitem" disabled={busy} onClick={() => void dismiss(it)}>
                          {t('不用做了')}
                        </button>
                        <button type="button" role="menuitem" disabled={busy} onClick={() => void markDone(it)}>
                          {t('已經好了')}
                        </button>
                      </div>
                    ) : null}
                  </li>
                ))}
              </ul>
            )}

            {adding ? (
              <div className="ledger-add">
                <input
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  placeholder={t('一句話說明漏了什麼')}
                  aria-label={t('補漏的內容')}
                  autoFocus
                />
                <button type="button" disabled={busy || !draft.trim()} onClick={() => void addItem()}>
                  {t('新增')}
                </button>
                <button type="button" disabled={busy} onClick={() => setAdding(false)}>
                  {t('取消')}
                </button>
              </div>
            ) : (
              <button type="button" className="ledger-add-open" disabled={busy} onClick={() => setAdding(true)}>
                {t('＋ 漏了什麼？補一項')}
              </button>
            )}

            {allUntriaged ? (
              <button type="button" className="ledger-dismiss" disabled={busy} onClick={() => void dismissAll()}>
                {t('這則不用追蹤')}
              </button>
            ) : null}
          </div>
        </>
      ) : null}
    </div>
  )
}

function statusLabel(s: LedgerItemStatus, t: ReturnType<typeof useT>): string {
  switch (s) {
    case 'untriaged':
      return t('待整理')
    case 'following':
      return t('進行中')
    case 'clarifying':
      return t('需要你回覆')
    case 'answered':
      return t('已回答')
    case 'done':
      return t('已完成')
    case 'cancelled':
      return t('不用做了')
    case 'merged':
      return t('已併入')
  }
}
