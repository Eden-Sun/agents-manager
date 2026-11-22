import { useEffect, useState, type ReactNode } from 'react'
import { createPortal } from 'react-dom'
import { completeLedgerItem, fetchProjectLedger, notSureLedgerItem, patchLedgerItem } from '../api/ledger'
import type { LedgerDeployState, LedgerItem, LedgerItemStatus, Message } from '../api/types'
import { ApiError } from '../api/types'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { AwaitingActions } from './AwaitingActions'
import { useNotSureNext } from '../lib/ledgerNotSure'
import './messageLedgerBar.css'
import './trackingBar.css'

const COLLAPSE_KEY = 'am-tracking-collapsed'
const RANK: Partial<Record<LedgerItemStatus, number>> = { answered: 0, clarifying: 1, untriaged: 2, following: 3 }

function readCollapsed(): boolean {
  try {
    return localStorage.getItem(COLLAPSE_KEY) === '1'
  } catch {
    return false
  }
}

function writeCollapsed(v: boolean) {
  try {
    localStorage.setItem(COLLAPSE_KEY, v ? '1' : '0')
  } catch {
  }
}

function messageTime(messages: Record<string, Message[]>, messageId: string | null): { at: string | null; botId: string | null } {
  if (!messageId) return { at: null, botId: null }
  for (const [botId, list] of Object.entries(messages)) {
    const hit = list.find((m) => m.id === messageId)
    if (hit) return { at: hit.created_at, botId }
  }
  return { at: null, botId: null }
}

export function TrackingBar({ botId, onJump }: { botId: string; onJump: (messageId: string) => boolean }) {
  const t = useT()
  const notSureNext = useNotSureNext()
  const projectId = useStore((s) => s.bots.find((b) => b.id === botId)?.project_id ?? null)
  const bots = useStore((s) => s.bots)
  const anyRev = useStore((s) => s.ledgerAnyRev)
  const notify = useStore((s) => s.notify)
  const [items, setItems] = useState<LedgerItem[] | null>(null)
  const [collapsed, setCollapsed] = useState<boolean>(readCollapsed)
  const [sheet, setSheet] = useState(false)
  const [menuFor, setMenuFor] = useState<string | null>(null)
  const [hintFor, setHintFor] = useState<{ id: string; text: string } | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    if (!projectId) {
      setItems(null)
      return
    }
    let alive = true
    void fetchProjectLedger(projectId, { status: 'open' })
      .then((list) => alive && setItems(list))
      .catch(() => alive && setItems(null))
    return () => {
      alive = false
    }
  }, [projectId, anyRev])

  if (!projectId || !items || items.length === 0) return null

  const sorted = [...items].sort((a, b) => (RANK[a.status] ?? 9) - (RANK[b.status] ?? 9) || (a.updated_at < b.updated_at ? 1 : -1))
  const needReply = items.filter((it) => it.status === 'clarifying').length
  const awaiting = items.filter((it) => it.status === 'answered').length
  const summary = [
    t('追蹤中 {n} 項', { n: items.length }),
    awaiting > 0 ? t('待你驗收 {a}', { a: awaiting }) : null,
    needReply > 0 ? t('需要你回覆 {m}', { m: needReply }) : null,
  ]
    .filter(Boolean)
    .join('・')

  const toggleCollapsed = (v: boolean) => {
    setCollapsed(v)
    writeCollapsed(v)
    if (v) setSheet(false)
  }

  const jump = (item: LedgerItem) => {
    if (!item.source_message_id) return
    if (onJump(item.source_message_id)) {
      setSheet(false)
      setHintFor(null)
      return
    }
    const where = messageTime(useStore.getState().messages, item.source_message_id)
    const text =
      where.botId && where.botId !== botId
        ? t('這則在 {bot} 的對話裡', { bot: bots.find((b) => b.id === where.botId)?.name ?? where.botId })
        : t('這則訊息還沒載入，往上捲一下再試')
    setHintFor({ id: item.id, text })
  }

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true)
    try {
      await fn()
      setMenuFor(null)
      setItems(await fetchProjectLedger(projectId, { status: 'open' }))
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        notify('error', t('這一項已被別人更新，已重新載入，請再操作一次'))
        setItems(await fetchProjectLedger(projectId, { status: 'open' }).catch(() => items))
      } else {
        notify('error', t('帳本操作失敗'))
      }
    } finally {
      setBusy(false)
    }
  }

  if (collapsed) {
    return (
      <div className="track-bar track-bar-collapsed">
        <button type="button" className="track-pill" aria-expanded={false} onClick={() => toggleCollapsed(false)}>
          <span aria-hidden="true">📋</span> {t('追蹤中')} {items.length} <span aria-hidden="true">▾</span>
        </button>
      </div>
    )
  }

  return (
    <div className="track-bar">
      <div className="track-line">
        <button type="button" className="track-summary" aria-expanded={sheet} onClick={() => setSheet((v) => !v)}>
          <span aria-hidden="true">📋</span> {summary}
        </button>
        <button type="button" className="track-collapse" aria-label={t('收合追蹤列')} onClick={() => toggleCollapsed(true)}>
          ▴
        </button>
      </div>
      {sheet ? sheetLayer(
        <>
          <div className="ledger-backdrop" aria-hidden="true" onClick={() => setSheet(false)} />
          <div className="ledger-panel track-panel" role="dialog" aria-label={t('本專案的追蹤任務')} onKeyDown={(e) => e.key === 'Escape' && setSheet(false)}>
            <div className="ledger-panel-head">
              <strong>{t('本專案的追蹤任務')}</strong>
              <button type="button" className="ledger-close" aria-label={t('關閉')} onClick={() => setSheet(false)}>
                ×
              </button>
            </div>
            <ul className="ledger-rows">
              {sorted.map((it) => {
                const when = messageTime(useStore.getState().messages, it.source_message_id)
                const at = when.at ? new Date(when.at).toLocaleString([], { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' }) : null
                return (
                  <li key={it.id} className={`ledger-row status-${it.status}`}>
                    <div className="ledger-code-col">
                      <span className={`ledger-code status-${it.status}`}>{it.code}</span>
                    </div>
                    <div className="ledger-body-col">
                      <div className="ledger-row-main">
                        <button type="button" className="track-jump" disabled={!it.source_message_id} onClick={() => jump(it)}>
                          <span className="ledger-row-title">{it.title}</span>
                        </button>
                        <span className={`ledger-chip status-${it.status}`}>{statusLabel(it.status, t, it.deploy_state)}</span>
                        {it.status === 'answered' ? null : (
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
                        )}
                      </div>
                      {it.status === 'answered' && it.deliverable ? <div className="ledger-row-meta"><span className="ledger-row-note">{t('交件內容')}：{it.deliverable}</span></div> : null}
                      {it.status === 'answered' ? (
                        <AwaitingActions
                          busy={busy}
                          onDone={() => void run(() => completeLedgerItem(projectId, it))}
                          onNotSure={() => void run(() => notSureLedgerItem(projectId, it, notSureNext))}
                        />
                      ) : null}
                      <div className="ledger-row-meta">
                        {it.next_step ? <span className="ledger-row-note">{t('下一步')}：{it.next_step}</span> : null}
                        {at ? <span className="ledger-row-owner">{t('訊息時間')}：{at}</span> : null}
                      </div>
                      {hintFor && hintFor.id === it.id ? <div className="track-hint">{hintFor.text}</div> : null}
                      {menuFor === it.id ? (
                        <div className="ledger-menu" role="menu">
                          <button
                            type="button"
                            role="menuitem"
                            disabled={busy}
                            onClick={() => void run(() => patchLedgerItem(projectId, it.id, { op: 'cancel', expected_version: it.version, close_reason: 'cancelled by user' }))}
                          >
                            {t('不用做了')}
                          </button>
                          <button
                            type="button"
                            role="menuitem"
                            disabled={busy}
                            onClick={() => void run(() => patchLedgerItem(projectId, it.id, { op: 'update', expected_version: it.version, status: 'done' }))}
                          >
                            {t('已經好了')}
                          </button>
                        </div>
                      ) : null}
                    </div>
                  </li>
                )
              })}
            </ul>
          </div>
        </>,
      ) : null}
    </div>
  )
}

function sheetLayer(node: ReactNode) {
  const phone = typeof window !== 'undefined' && window.matchMedia?.('(max-width: 600px)').matches
  return phone ? createPortal(node, document.body) : node
}

function statusLabel(status: LedgerItemStatus, t: ReturnType<typeof useT>, deploy?: LedgerDeployState | null): string {
  switch (status) {
    case 'clarifying':
      return t('需要你回覆')
    case 'untriaged':
      return t('已加入追蹤')
    case 'following':
      return t('進行中')
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
