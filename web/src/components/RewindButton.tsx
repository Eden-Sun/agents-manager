import { useT } from '../lib/i18n'
import { useState } from 'react'
import type { Bot, Message } from '../api/types'
import { canOfferRewind, rewindBlocked } from '../lib/rewind'
import { rewindAndRefill } from '../store/rewindAction'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import './rewind.css'

export function RewindButton({
 msg }: { msg: Message }) {
  const botId = msg.bot_id ?? null
  const offer = useStore((s) => Boolean(botId) && !msg.group_id && canOfferRewind(msg, s.bots.find((b) => b.id === botId)))
  return offer ? <RewindActive msg={msg} /> : null
}

function RewindActive({ msg }: { msg: Message }) {
  const botId = msg.bot_id ?? null
  const bot = useStore((s) => (botId ? (s.bots.find((b) => b.id === botId) ?? null) : null))
  const blocked = useStore((s) => (botId ? rewindBlocked(s.runs[botId]) : null))
  const afterOf = () => {
    const list = botId ? (useStore.getState().messages[botId] ?? []) : []
    const i = list.findIndex((m) => m.id === msg.id)
    return i < 0 ? 0 : list.slice(i + 1).filter((m) => m.role !== 'system' && !m.rewound_at).length
  }
  return <RewindControl msg={msg} bot={bot} blocked={blocked} afterOf={afterOf} />
}

export function RewindControl({ msg, bot, blocked, after = 0, afterOf }: { msg: Message; bot: Bot | null; blocked: string | null; after?: number; afterOf?: () => number }) {
  const t = useT()
  const botId = msg.bot_id ?? null
  const [confirming, setConfirming] = useState(false)
  const [busy, setBusy] = useState(false)
  const [draft, setDraft] = useState<string | null>(null)
  if (!botId || msg.group_id || !canOfferRewind(msg, bot)) return null

  const go = (clear?: string) => {
    setConfirming(false)
    setDraft(null)
    setBusy(true)
    void rewindAndRefill(botId, msg.id, clear)
      .then((r) => {
        if (typeof r === 'object') setDraft(r.composerDraft)
      })
      .finally(() => setBusy(false))
  }

  const name = bot?.name ?? t('這個 Bot')
  return (
    <>
      <button
        type="button"
        className={`msg-rewind${busy ? ' busy' : ''}`}
        disabled={busy || blocked !== null}
        aria-label={t('倒回到這則之前（{name}）', { name })}
        title={blocked ?? t('倒回到這則之前：這則與之後的問答從對話脈絡拿掉，原文放回輸入框')}
        onClick={() => setConfirming(true)}
      >
        {busy ? t('倒回中…') : t('↶ 倒回這裡')}
      </button>
      <RewindConfirm open={confirming} name={name} after={confirming && afterOf ? afterOf() : after} onCancel={() => setConfirming(false)} onConfirm={() => go()} />
      <ConfirmDialog
        open={draft !== null}
        title={t("終端輸入列裡有字")}
        body={
          <>
            <p>
              {t.x('{name} 的終端輸入列裡有一段還沒送出的字，倒回前要先清掉它：', { name: <strong>{name}</strong> })}
            </p>
            <pre className="rewind-draft">{draft}</pre>
            <p>{t('清掉的字不會送出，也不會放回這裡的輸入框。')}</p>
          </>
        }
        confirmLabel={t("清掉再倒回")}
        danger
        width={420}
        onCancel={() => setDraft(null)}
        onConfirm={() => go(draft ?? '')}
      />
    </>
  )
}

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
  const t = useT()
  return (
    <ConfirmDialog
      open={open}
      title={t("倒回到這則之前？")}
      body={
        <>
          <p>
            {after > 0
            ? t.x('這則與之後的 {n} 則會從 {name} 的對話脈絡裡拿掉（在終端用 claude 的 /rewind，不重啟），接著這則之前的對話繼續；這則的原文會放回輸入框讓你改寫。', { n: after, name: <strong>{name}</strong> })
            : t.x('這則會從 {name} 的對話脈絡裡拿掉（在終端用 claude 的 /rewind，不重啟），接著這則之前的對話繼續；這則的原文會放回輸入框讓你改寫。', { name: <strong>{name}</strong> })}
          </p>
          <p>{t('程式碼與檔案不會跟著還原。網頁上的紀錄保留（收成「已倒回」）。')}</p>
        </>
      }
      confirmLabel={t("倒回")}
      danger
      width={420}
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  )
}

export function RewoundTag({ at }: { at: string }) {
  const t = useT()
  return (
    <span className="src-tag rewound-tag" title={t('{at} 倒回：這則已不在 bot 的對話脈絡裡', { at })}>
      {t('已倒回')}
    </span>
  )
}
