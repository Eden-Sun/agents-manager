import { useState } from 'react'
import type { Bot, Message } from '../api/types'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { canOfferRewind, rewindBlocked } from '../lib/rewind'
import { rewindAndRefill } from '../store/rewindAction'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import './rewind.css'

/**
 * 使用者訊息上的「倒回這裡」（SPEC §6.13，issue #405）：把 claude 的對話倒回到這則送出**之前**，
 * 這則的原文放回輸入框。daemon 在終端驅動 claude 自己的 `/rewind`（不重啟、同一段對話分支）。一定先確認：這則之後的問答會從它的
 * 對話脈絡裡拿掉（紀錄還留著、收成「已倒回」）。
 * 群組發言（`group_id`）不給：那一則同時送給好幾顆，從這裡倒只會倒掉其中一顆，對不上群組的時間線。
 * **只在手機畫**：桌機是輸入列旁那顆「⟲ 倒回」（`RewindBar`，使用者 2026-09-24：滑過才出現的按鈕找不到）。
 */
export function RewindButton({ msg }: { msg: Message }) {
  const phone = useMediaQuery(PHONE_QUERY)
  const botId = msg.bot_id ?? null
  const bot = useStore((s) => (botId ? (s.bots.find((b) => b.id === botId) ?? null) : null))
  const blocked = useStore((s) => (botId ? rewindBlocked(s.runs[botId]) : null))
  // 這則之後還有幾則會一起被拿掉（確認框講清楚代價）。
  const after = useStore((s) => {
    const list = botId ? (s.messages[botId] ?? []) : []
    const i = list.findIndex((m) => m.id === msg.id)
    return i < 0 ? 0 : list.slice(i + 1).filter((m) => m.role !== 'system' && !m.rewound_at).length
  })
  if (!phone) return null
  return <RewindControl msg={msg} bot={bot} blocked={blocked} after={after} />
}

/** 不讀 store 的那一半（測試直接餵 props；SSR 渲染拿不到 `useStore.setState` 的狀態）。 */
export function RewindControl({ msg, bot, blocked, after }: { msg: Message; bot: Bot | null; blocked: string | null; after: number }) {
  const botId = msg.bot_id ?? null
  const [confirming, setConfirming] = useState(false)
  const [busy, setBusy] = useState(false)
  if (!botId || msg.group_id || !canOfferRewind(msg, bot)) return null

  const go = () => {
    setConfirming(false)
    setBusy(true)
    void rewindAndRefill(botId, msg.id).finally(() => setBusy(false))
  }

  const name = bot?.name ?? '這個 Bot'
  return (
    <>
      <button
        type="button"
        className={`msg-rewind${busy ? ' busy' : ''}`}
        disabled={busy || blocked !== null}
        aria-label={`倒回到這則之前（${name}）`}
        title={blocked ?? '倒回到這則之前：這則與之後的問答從對話脈絡拿掉，原文放回輸入框'}
        onClick={() => setConfirming(true)}
      >
        {busy ? '倒回中…' : '↶ 倒回這裡'}
      </button>
      <RewindConfirm open={confirming} name={name} after={after} onCancel={() => setConfirming(false)} onConfirm={go} />
    </>
  )
}

/** 確認框：手機每則的按鈕與桌機的「⟲ 倒回」共用。`after`＝那一則之後還會一起拿掉幾則。 */
export function RewindConfirm({
  open,
  name,
  after,
  onCancel,
  onConfirm,
}: {
  open: boolean
  name: string
  after: number
  onCancel: () => void
  onConfirm: () => void
}) {
  return (
    <ConfirmDialog
      open={open}
      title="倒回到這則之前？"
      body={
        <>
          <p>
            這則{after > 0 ? `與之後的 ${after} 則` : ''}會從 <strong>{name}</strong> 的對話脈絡裡拿掉（在終端用 claude 的 /rewind，不重啟），接著這則之前的對話繼續；這則的原文會放回輸入框讓你改寫。
          </p>
          <p>程式碼與檔案不會跟著還原。網頁上的紀錄保留（收成「已倒回」）。</p>
        </>
      }
      confirmLabel="倒回"
      danger
      width={420}
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  )
}

/** 已經被倒掉的訊息：meta 上的小標。 */
export function RewoundTag({ at }: { at: string }) {
  return (
    <span className="src-tag rewound-tag" title={`${at} 倒回：這則已不在 bot 的對話脈絡裡`}>
      已倒回
    </span>
  )
}
