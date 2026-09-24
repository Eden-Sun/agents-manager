import { useState } from 'react'
import type { Bot, Message, Run } from '../api/types'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { rewindBarBlocked, rewindCandidates, rewindCost, rewindPreview } from '../lib/rewind'
import { rewindAndRefill } from '../store/rewindAction'
import { useStore } from '../store/store'
import { Modal } from './Modal'
import { RewindConfirm } from './RewindButton'
import './rewind.css'

/**
 * 桌機：輸入列旁邊一顆常駐的「⟲ 倒回」（使用者 2026-09-24：「沒看見 pc 版做成一顆按鈕在對話下方」——滑過才出現的
 * 每則按鈕找不到）。按下去挑一則（新到舊、預設最新）→ 同一個確認框 → `POST /bots/:id/rewind`。手機維持每則常駐的按鈕。
 */
export function RewindBar({ botId }: { botId: string }) {
  const phone = useMediaQuery(PHONE_QUERY)
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const run = useStore((s) => s.runs[botId] ?? null)
  const list = useStore((s) => s.messages[botId])
  if (phone) return null
  return <RewindBarView botId={botId} bot={bot} run={run} messages={list ?? []} />
}

type Stage = { kind: 'closed' } | { kind: 'pick'; id: string } | { kind: 'confirm'; id: string }

/** 不讀 store 的那一半（測試直接餵 props）；`stage` 給測試指定一開始停在哪一步。 */
export function RewindBarView({
  botId,
  bot,
  run,
  messages,
  stage: initial = { kind: 'closed' },
}: {
  botId: string
  bot: Bot | null
  run: Run | null
  messages: Message[]
  stage?: Stage
}) {
  const [stage, setStage] = useState<Stage>(initial)
  const [busy, setBusy] = useState(false)
  const candidates = rewindCandidates(messages)
  const blocked = rewindBarBlocked(bot, run, candidates.length)
  const name = bot?.name ?? '這個 Bot'

  const go = (id: string) => {
    setStage({ kind: 'closed' })
    setBusy(true)
    void rewindAndRefill(botId, id).finally(() => setBusy(false))
  }

  return (
    <>
      <button
        type="button"
        className="rewind-bar-btn"
        disabled={busy || blocked !== null}
        aria-label="倒回對話：挑一則使用者訊息，倒回到送出它之前"
        title={blocked ?? '倒回對話：挑一則使用者訊息，那則與之後的問答從脈絡拿掉，原文放回輸入框'}
        onClick={() => candidates[0] && setStage({ kind: 'pick', id: candidates[0].id })}
      >
        {busy ? '倒回中…' : '⟲ 倒回'}
      </button>
      <Modal open={stage.kind === 'pick'} title="倒回到哪一則之前？" subtitle={name} width={520} onClose={() => setStage({ kind: 'closed' })}>
        <RewindPicker items={candidates} selected={stage.kind === 'pick' ? stage.id : ''} onSelect={(id) => setStage({ kind: 'pick', id })} />
        <div className="rewind-pick-actions">
          <button type="button" className="mini-btn" onClick={() => setStage({ kind: 'closed' })}>
            取消
          </button>
          <button type="button" className="mini-btn primary" disabled={stage.kind !== 'pick'} onClick={() => stage.kind === 'pick' && setStage({ kind: 'confirm', id: stage.id })}>
            下一步
          </button>
        </div>
      </Modal>
      <RewindConfirm
        open={stage.kind === 'confirm'}
        name={name}
        after={stage.kind === 'confirm' ? rewindCost(messages, stage.id) - 1 : 0}
        onCancel={() => setStage({ kind: 'closed' })}
        onConfirm={() => stage.kind === 'confirm' && go(stage.id)}
      />
    </>
  )
}

/** 清單：新到舊，每則前兩行＋時間，單選。 */
export function RewindPicker({ items, selected, onSelect }: { items: Message[]; selected: string; onSelect: (id: string) => void }) {
  return (
    <div className="rewind-pick" role="radiogroup" aria-label="要倒回的使用者訊息">
      {items.map((m) => (
        <label key={m.id} className={`rewind-pick-item${m.id === selected ? ' on' : ''}`}>
          <input type="radio" name="rewind-pick" checked={m.id === selected} onChange={() => onSelect(m.id)} />
          <span className="rewind-pick-text">{rewindPreview(m.content)}</span>
          <time className="rewind-pick-time" dateTime={m.created_at}>
            {clock(m.created_at)}
          </time>
        </label>
      ))}
    </div>
  )
}

function clock(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return ''
  const today = new Date().toDateString() === d.toDateString()
  const hm = `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
  return today ? hm : `${d.getMonth() + 1}/${d.getDate()} ${hm}`
}
