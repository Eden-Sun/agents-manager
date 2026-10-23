import { useT } from '../lib/i18n'
import { useState } from 'react'
import type { Bot, Message } from '../api/types'
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
 * 桌機手機都畫、常駐不必滑過（使用者 2026-09-28：「rewind 還是綁在對話上比較合適，而不是多一個倒回鍵」——取代 09-24 輸入列旁的「⟲ 倒回」）。
 */
export function RewindButton({
 msg }: { msg: Message }) {
  const botId = msg.bot_id ?? null
  // 每顆使用者泡泡都掛這一個：只回一個布林。不可倒回的（群組發言、非 claude、已倒回過）就到此為止，不再多掛 selector——
  // 清單有幾百則時，每次 store 更新每個 selector 都要跑一遍。
  const offer = useStore((s) => Boolean(botId) && !msg.group_id && canOfferRewind(msg, s.bots.find((b) => b.id === botId)))
  return offer ? <RewindActive msg={msg} /> : null
}

function RewindActive({ msg }: { msg: Message }) {
  const botId = msg.bot_id ?? null
  const bot = useStore((s) => (botId ? (s.bots.find((b) => b.id === botId) ?? null) : null))
  const blocked = useStore((s) => (botId ? rewindBlocked(s.runs[botId]) : null))
  // 「這則之後還有幾則會一起被拿掉」只有確認框要用：按下去才數（以前每個 selector 在每次 store 更新時
  // 各掃一遍整段清單，N 則就是 O(N²)）。
  const afterOf = () => {
    const list = botId ? (useStore.getState().messages[botId] ?? []) : []
    const i = list.findIndex((m) => m.id === msg.id)
    return i < 0 ? 0 : list.slice(i + 1).filter((m) => m.role !== 'system' && !m.rewound_at).length
  }
  return <RewindControl msg={msg} bot={bot} blocked={blocked} afterOf={afterOf} />
}

/** 不讀 store 的那一半（測試直接餵 props；SSR 渲染拿不到 `useStore.setState` 的狀態）。 */
export function RewindControl({ msg, bot, blocked, after = 0, afterOf }: { msg: Message; bot: Bot | null; blocked: string | null; after?: number; afterOf?: () => number }) {
  const t = useT()
  const botId = msg.bot_id ?? null
  const [confirming, setConfirming] = useState(false)
  const [busy, setBusy] = useState(false)
  /** 終端輸入列卡著的那段字（409 `composer_busy`）：非 null＝在問要不要清掉再倒回。 */
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

/** 確認框。`after`＝那一則之後還會一起拿掉幾則。 */
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

/** 已經被倒掉的訊息：meta 上的小標。 */
export function RewoundTag({ at }: { at: string }) {
  const t = useT()
  return (
    <span className="src-tag rewound-tag" title={t('{at} 倒回：這則已不在 bot 的對話脈絡裡', { at })}>
      {t('已倒回')}
    </span>
  )
}
