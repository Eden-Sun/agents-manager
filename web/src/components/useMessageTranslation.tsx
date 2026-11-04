import { useEffect, useSyncExternalStore } from 'react'
import { translateMessage } from '../api'
import type { Message } from '../api/types'
import { isMostlyEnglish } from '../lib/isMostlyEnglish'
import { useT } from '../lib/i18n'
import { claimAutoTranslate, getTranslation, isWanted, setTranslation, setWanted, subscribeTranslations } from '../lib/messageTranslationStore'
import { useStore } from '../store/store'
import { SafeMarkdown } from './SafeMarkdown'
import { resetTranslateReady, useTranslateReady } from './useTranslateReady'

/** 美元：不到一分錢多給位數，免得全寫成 $0.00。 */
export function usd(n: number): string {
  return n >= 0.01 ? `$${n.toFixed(3)}` : `$${n.toFixed(5)}`
}

/** 譯文下方那行：這次翻譯花多少（使用者 2026-11-04：即時算出該次翻譯價格）；快取命中標未扣費。 */
function costLine(t: ReturnType<typeof useT>, st: { cost: number | null; cached: boolean; model: string }): string {
  const model = st.model.replace(/^claude-/, '')
  if (st.cached) return st.cost === null ? t('{model} · 快取，這次未扣費', { model }) : t('{model} · 快取，這次未扣費（當初 {cost}）', { model, cost: usd(st.cost) })
  return st.cost === null ? t('{model} · 費用不明', { model }) : t('{model} · 這次翻譯 {cost}', { model, cost: usd(st.cost) })
}

/**
 * assistant 訊息說英文時，meta 列的「翻譯」鈕＋譯文（daemon 用 Claude API 翻成繁中，同一則有快取不重複扣費）。
 * 譯文跟原文在**同一個泡泡**裡翻面切換（使用者 2026-11-04：不要在下面多一塊，要像翻面）：`content` 不是 null 時，
 * 呼叫端用它取代泡泡裡的原文。`flip` 沒翻過是 null，翻過後是正在看的那一面（tr／orig），呼叫端拿來當 key 重播翻面動畫。不是英文就全是 null。
 */
export function useMessageTranslation(msg: Message): { button: React.ReactNode; content: React.ReactNode; flip: 'tr' | 'orig' | null } {
  const t = useT()
  const st = useSyncExternalStore(subscribeTranslations, () => getTranslation(msg.id), () => getTranslation(msg.id))
  const eligible = msg.role === 'assistant' && !msg.rewound_at && msg.source !== 'terminal_fallback' && isMostlyEnglish(msg.content)
  const ready = useTranslateReady(eligible)
  const off = ready && !ready.ok ? ready : null

  // 狀態在 store（以 msg.id 為鍵）：remount 不會掉回原文。auto＝重整後自動重抓，失敗不跳錯誤也不重試。
  const run = (auto: boolean) => {
    setTranslation(msg.id, { status: 'loading' })
    translateMessage(msg.id).then(
      (r) => setTranslation(msg.id, { status: 'done', text: r.translation, show: !auto || isWanted(msg.id), cost: r.costUsd, cached: r.cached, model: r.model }),
      (e: unknown) => {
        setTranslation(msg.id, { status: 'error' })
        resetTranslateReady()
        if (auto) return
        const reason = e instanceof Error ? e.message : String(e)
        // 未設定時直接講去哪裡開，不丟錯誤碼。
        useStore.getState().notify('error', reason.includes('claude_api_not_configured')
          ? t('翻譯沒有開：到「環境設定 → Claude API」打開總開關與「對話訊息翻譯」，並確認 API key 已設定')
          : t('翻譯失敗：{reason}', { reason }))
      },
    )
  }
  const flipFace = () => {
    if (st.status !== 'done') return
    setWanted(msg.id, !st.show)
    setTranslation(msg.id, { ...st, show: !st.show })
  }
  // 以前選了看譯文（localStorage 有記）→ 掛載時自動翻一次；載入中顯示原文＋「翻譯中…」。
  const auto = eligible && !off && st.status === 'idle' && isWanted(msg.id)
  useEffect(() => {
    if (auto && claimAutoTranslate(msg.id)) run(true)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [auto, msg.id])
  if (!eligible) return { button: null, content: null, flip: null }
  // 未啟用：按鈕照樣在（找得到這個功能），標「未啟用」，按了只提示去哪裡開，不打 API。
  const onClick = off
    ? () => useStore.getState().notify('info', t.tr(off.reason))
    : st.status === 'done' ? flipFace : st.status === 'loading' ? undefined : () => { setWanted(msg.id, true); run(false) }
  // 翻好之後按鈕是「翻面」：正看譯文就寫「原文」，正看原文就寫「譯文」。
  const label = off ? t('翻譯（未啟用）') : st.status === 'loading' ? t('翻譯中…') : st.status === 'done' ? (st.show ? t('⇄ 原文') : t('⇄ 譯文')) : t('翻譯')
  return {
    button: (
      <button type="button" className={`msg-translate${off ? ' off' : ''}`} disabled={st.status === 'loading'} onClick={onClick} title={off ? t.tr(off.reason) : t('用 Claude 翻成繁體中文')}>
        {label}
      </button>
    ),
    content:
      st.status === 'done' && st.show ? (
        <div className="msg-translation" lang="zh-Hant">
          <SafeMarkdown text={st.text} botId={msg.bot_id} />
          <div className="msg-translation-cost">{costLine(t, st)}</div>
        </div>
      ) : null,
    flip: st.status !== 'done' ? null : st.show ? 'tr' : 'orig',
  }
}
