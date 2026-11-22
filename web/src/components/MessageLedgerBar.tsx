import { useEffect, useState } from 'react'
import { completeLedgerItem, createLedgerItems, fetchProjectLedger, notSureLedgerItem, patchLedgerItem } from '../api/ledger'
import type { LedgerDeployState, LedgerItem, LedgerItemStatus, Message } from '../api/types'
import { ApiError } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { AwaitingActions } from './AwaitingActions'
import { useNotSureNext } from '../lib/ledgerNotSure'
import './messageLedgerBar.css'

const SUMMARY_ORDER: LedgerItemStatus[] = ['answered', 'clarifying', 'untriaged', 'following', 'done']
const ACTIONABLE: LedgerItemStatus[] = ['untriaged', 'following', 'clarifying']

export function MessageLedgerBar({ msg }: { msg: Message }) {
  const t = useT()
  const notSureNext = useNotSureNext()
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
  const [addMenu, setAddMenu] = useState(false)

  useEffect(() => {
    if (msg.ledger_items) setItems(msg.ledger_items)
  }, [msg.ledger_items])

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

  if (!projectId) return null

  const all = items ?? []
  const live = all.filter((it) => it.status !== 'merged' && it.status !== 'cancelled')
  const allUntriaged = live.every((it) => it.status === 'untriaged')
  const allClosed = live.every((it) => it.status === 'done')

  const counts: Partial<Record<LedgerItemStatus, number>> = {}
  for (const it of live) counts[it.status] = (counts[it.status] ?? 0) + 1
  const parts = SUMMARY_ORDER.filter((s) => (counts[s] ?? 0) > 0).map((s) => `${s === 'answered' ? t('待你驗收') : statusLabel(s, t)} ${counts[s]}`)

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

  const markDone = (item: LedgerItem) => runPatch(() => completeLedgerItem(projectId, item))
  const notSure = (item: LedgerItem) => runPatch(() => notSureLedgerItem(projectId, item, notSureNext))
  const dismiss = (item: LedgerItem) =>
    runPatch(() => patchLedgerItem(projectId, item.id, { op: 'cancel', expected_version: item.version, close_reason: 'cancelled by user' }))
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

  const addToTracking = () =>
    runPatch(async () => {
      const title = trackingTitle(msg.content)
      if (!title) return null
      const created = await createLedgerItems(projectId, { source_message_id: msg.id, title, status: 'untriaged' })
      setItems((cur) => [...(cur ?? []), ...created])
      setAddMenu(false)
      notify('info', t('已加入追蹤，交管理者整理'))
      return null
    })

  const addRow =
    msg.role === 'user' ? (
      <div className="ledger-add-row">
        <button type="button" className="ledger-more" aria-label={t('訊息動作')} aria-haspopup="menu" aria-expanded={addMenu} onClick={() => setAddMenu((v) => !v)}>
          ⋯
        </button>
        {addMenu ? (
          <div className="ledger-menu ledger-add-menu" role="menu">
            {live.length > 0 ? (
              <button type="button" role="menuitem" disabled>
                {t('已在追蹤')}
              </button>
            ) : (
              <button type="button" role="menuitem" disabled={busy || !trackingTitle(msg.content)} onClick={() => void addToTracking()}>
                {t('加入追蹤')}
              </button>
            )}
          </div>
        ) : null}
      </div>
    ) : null

  return (
    <>
    {live.length > 0 ? (
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
                    <div className="ledger-code-col">
                      <span className="ledger-code">{it.code}</span>
                    </div>
                    <div className="ledger-body-col">
                      <div className="ledger-row-main">
                        <span className="ledger-row-title">{it.title}</span>
                        <span className={`ledger-chip status-${it.status}`}>{statusLabel(it.status, t, it.deploy_state)}</span>
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
                      {it.status === 'answered' && it.deliverable ? <div className="ledger-row-note">{t('交件內容')}：{it.deliverable}</div> : null}
                      {it.next_step ? <div className="ledger-row-note">{t('下一步')}：{it.next_step}</div> : null}
                      {it.owner_bot_id ? <div className="ledger-row-owner">{t('承辦')}：{botName(it.owner_bot_id)}</div> : null}
                      {it.status === 'answered' ? (
                        <AwaitingActions busy={busy} onDone={() => void markDone(it)} onNotSure={() => void notSure(it)} />
                      ) : null}
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
                    </div>
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
    ) : null}
    {addRow}
    </>
  )
}

function trackingTitle(content: string): string {
  const first = (content.trim().split('\n')[0] ?? '').trim()
  return Array.from(first).slice(0, 80).join('')
}

function statusLabel(status: LedgerItemStatus, t: ReturnType<typeof useT>, deploy?: LedgerDeployState | null): string {
  switch (status) {
    case 'untriaged':
      return t('待整理')
    case 'following':
      return t('進行中')
    case 'clarifying':
      return t('需要你回覆')
    case 'answered':
      return deploy === 'pending' ? t('等待部署') : deploy === 'live' ? t('已部署待驗收') : t('待驗收')
    case 'done':
      return t('已完成')
    case 'cancelled':
      return t('不用做了')
    case 'merged':
      return t('已併入')
  }
}
