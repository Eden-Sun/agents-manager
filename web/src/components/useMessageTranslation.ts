import { createElement, useEffect, useState, useSyncExternalStore } from 'react'
import { reportTranslation, translateMessage } from '../api'
import { ApiError } from '../api/types'
import type { Message } from '../api/types'
import { isMostlyEnglish } from '../lib/isMostlyEnglish'
import { isMostlySimplified } from '../lib/zhSimplified'
import { useT } from '../lib/i18n'
import { autoTranslateAllowed, type RejectDetail, claimAutoTranslate, getTranslation, isDeclined, isWanted, noteAutoFailure, noteTranslationSuccess, setDeclined, setTranslation, setWanted, subscribeTranslations } from '../lib/messageTranslationStore'
import { useStore } from '../store/store'
import { resetTranslateReady, useTranslateReady } from './useTranslateReady'
import { MessageTranslationButton, MessageTranslationContent } from './MessageTranslation'
import { AUTO_TRANSLATE_RECENT, usd } from './messageTranslationConstants'

export { AUTO_TRANSLATE_RECENT, usd }

/** 品質沒過的提示文字（手動翻、重新翻譯共用）。 */
function rejectNotice(t: ReturnType<typeof useT>, reason: string | undefined): string {
  return reason === 'japanese_output'
    ? t('譯文含日文，品質檢查未通過')
    : reason === 'untranslated'
      ? t('譯文跟原文幾乎一樣（沒翻成中文），品質檢查未通過')
      : reason === 'partially_untranslated'
        ? t('譯文仍有英文段落沒翻，品質檢查未通過')
        : t('這則沒有自動翻譯（品質檢查未通過），可以手動再試')
}

function isRejectDetail(v: unknown): v is RejectDetail {
  if (typeof v !== 'object' || v === null) return false
  const d = v as Record<string, unknown>
  return typeof d.check === 'string' && typeof d.identical === 'boolean' && Array.isArray(d.models) && typeof d.attempts === 'number'
}

/**
 * assistant 訊息說英文時，meta 列的「翻譯」鈕＋譯文（daemon 用 Claude API 翻成繁中，同一則有快取不重複扣費）。
 * 譯文跟原文在**同一個泡泡**裡翻面切換（使用者 2026-11-04：不要在下面多一塊，要像翻面）：`content` 不是 null 時，
 * 呼叫端用它取代泡泡裡的原文。
 * 自動翻譯（使用者 2026-11-06）：daemon 設定 `[claude_api.uses.message_translate] auto` 開著、用途啟用有 key、`recent`（呼叫端說這則在對話最後 N 則內）、
 * 訊息已完整（`!incomplete`，串流中不打 API）、使用者沒手動點回過原文時，掛載就自動翻並顯示譯文。同一則本頁只自動試一次，失敗停在可手動重試的狀態。
 * `flip` 沒翻過是 null，翻過後是正在看的那一面（tr／orig），呼叫端拿來當 key 重播翻面動畫。不是英文就全是 null。
 */
export function useMessageTranslation(
  msg: Message,
  opts: { recent?: boolean } = {},
): {
  button: React.ReactNode
  content: React.ReactNode
  flip: 'tr' | 'orig' | null
  animClass?: string
  swapKey?: string
} {
  const t = useT()
  const st = useSyncExternalStore(subscribeTranslations, () => getTranslation(msg.id), () => getTranslation(msg.id))
  const eligible = msg.role === 'assistant' && !msg.rewound_at && msg.source !== 'terminal_fallback' && (isMostlyEnglish(msg.content) || isMostlySimplified(msg.content))
  const ready = useTranslateReady(eligible)
  const off = ready && !ready.ok ? ready : null

  const [anim, setAnim] = useState<'forward' | 'backward' | 'auto' | null>(null)
  const [spin, setSpin] = useState<'forward' | 'backward' | null>(null)
  const [swapSeq, setSwapSeq] = useState(0)
  const [redoing, setRedoing] = useState(false)

  // 狀態在 store（以 msg.id 為鍵）：remount 不會掉回原文。
  // manual＝使用者按的；restore＝重整後重抓以前選過看譯文的；fresh＝自動翻譯新訊息。後兩者失敗不跳錯誤也不重試。
  const run = (kind: 'manual' | 'restore' | 'fresh') => {
    setTranslation(msg.id, { status: 'loading' })
    translateMessage(msg.id, kind === 'manual').then(
      (r) => {
        noteTranslationSuccess()
        const reduceMotion = typeof window !== 'undefined' && Boolean(window.matchMedia?.('(prefers-reduced-motion: reduce)').matches)

        if (kind === 'manual') {
          if (!reduceMotion) {
            setAnim('forward')
            setSpin('forward')
          }
          setSwapSeq((s) => s + 1)
          setTranslation(msg.id, { status: 'done', text: r.translation, show: true, ready: false, cost: r.costUsd, cached: r.cached, model: r.model, lowConfidence: r.lowConfidence, jevProb: r.jevProb })
          return
        }

        if (kind === 'restore') {
          const show = isWanted(msg.id)
          // 偏好恢復：初掛載與恢復偏好不播動畫、不轉圖示
          setTranslation(msg.id, { status: 'done', text: r.translation, show, ready: false, cost: r.costUsd, cached: r.cached, model: r.model, lowConfidence: r.lowConfidence, jevProb: r.jevProb })
          return
        }

        // kind === 'fresh'（自動翻譯新訊息）
        // 規格：貼底中只做 100ms 純淡入、不轉圖示；使用者往上讀這則／選取文字中時，先保留原文、按鈕顯示「譯文已就緒」
        const hasSelection = typeof window !== 'undefined' && Boolean(window.getSelection()?.toString().trim())
        const listEl = typeof document !== 'undefined' ? (document.querySelector('.msg-list') ?? document.querySelector('[data-list]')) : null
        const isReading = hasSelection || (listEl ? listEl.scrollHeight - listEl.scrollTop - listEl.clientHeight >= 30 : false)

        if (isReading) {
          setWanted(msg.id, true)
          setTranslation(msg.id, { status: 'done', text: r.translation, show: false, ready: true, cost: r.costUsd, cached: r.cached, model: r.model, lowConfidence: r.lowConfidence, jevProb: r.jevProb })
        } else {
          setWanted(msg.id, true)
          if (!reduceMotion) {
            setAnim('auto')
          }
          setSwapSeq((s) => s + 1)
          setTranslation(msg.id, { status: 'done', text: r.translation, show: true, ready: false, cost: r.costUsd, cached: r.cached, model: r.model, lowConfidence: r.lowConfidence, jevProb: r.jevProb })
        }
      },
      (e: unknown) => {
        // 品質檢查沒過：服務是好的，只是這則翻不好。不算失敗（不累計、不重置就緒狀態），顯示原文＋小提示。
        if (e instanceof ApiError && e.status === 422 && e.body.error === 'translate_rejected') {
          const reason = typeof e.body.reason === 'string' ? e.body.reason : undefined
          const text = typeof e.body.translation === 'string' && e.body.translation.trim() ? e.body.translation : undefined
          const detail = isRejectDetail(e.body.detail) ? e.body.detail : undefined
          setTranslation(msg.id, { status: 'rejected', reason, text, detail })
          // 手動按的要看到原因（不是靜默失敗）：提示列會寫檢查名稱，重試就再按「翻譯」，被擋的譯文可用「仍然顯示譯文」看。
          if (kind === 'manual') useStore.getState().notify('info', rejectNotice(t, reason))
          return
        }
        setTranslation(msg.id, { status: 'error' })
        resetTranslateReady()
        if (kind !== 'manual') {
          if (kind === 'fresh') noteAutoFailure()
          return
        }
        const reason = e instanceof Error ? e.message : String(e)
        // 未設定時直接講去哪裡開，不丟錯誤碼。
        // 翻譯設成用 OpenAI 但沒開或沒有金鑰：講清楚原因，daemon 不會悄悄改用 Claude。
        useStore.getState().notify('error', reason.includes('openai_not_configured')
          ? t('OpenAI 未啟用或未設定金鑰：翻譯的供應商設成 OpenAI，請啟用 OpenAI 並設定金鑰，或到「環境設定 → Claude API」把翻譯的供應商改回 Claude')
          : reason.includes('claude_api_not_configured')
          ? t('翻譯沒有開：到「環境設定 → Claude API」打開總開關與「對話訊息翻譯」，並確認 API key 已設定')
          : t('翻譯失敗：{reason}', { reason }))
      },
    )
  }
  // 重新翻譯：daemon 略過正面快取、用升級模型重翻。成功就換成新譯文（同一套交換動畫）；沒過（422）保留舊譯文＋提示原因。
  const redo = () => {
    if (redoing || st.status !== 'done') return
    setRedoing(true)
    translateMessage(msg.id, false, true).then(
      (r) => {
        setRedoing(false)
        noteTranslationSuccess()
        const reduceMotion = typeof window !== 'undefined' && Boolean(window.matchMedia?.('(prefers-reduced-motion: reduce)').matches)
        if (!reduceMotion) {
          setAnim('forward')
          setSpin('forward')
        }
        setSwapSeq((s) => s + 1)
        setTranslation(msg.id, { status: 'done', text: r.translation, show: true, ready: false, cost: r.costUsd, cached: r.cached, model: r.model, lowConfidence: r.lowConfidence, jevProb: r.jevProb })
      },
      (e: unknown) => {
        setRedoing(false)
        if (e instanceof ApiError && e.status === 422 && e.body.error === 'translate_rejected') {
          // 沒過就保留原本的譯文；原因（檢查名稱）看得到的是手動翻那條提示，這裡只說明沒換掉。
          useStore.getState().notify('info', t('重新翻譯沒過（品質檢查未通過），保留原本的譯文'))
          return
        }
        useStore.getState().notify('error', t('重新翻譯失敗：{reason}', { reason: e instanceof Error ? e.message : String(e) }))
      },
    )
  }
  const flipFace = () => {
    if (st.status !== 'done') return
    const reduceMotion = typeof window !== 'undefined' && Boolean(window.matchMedia?.('(prefers-reduced-motion: reduce)').matches)
    const isReady = Boolean(st.ready) && !st.show
    if (isReady) {
      // 點了「譯文已就緒」：使用者點了才換，不改寫 wanted/declined 偏好
      if (!reduceMotion) {
        setAnim('forward')
        setSpin('forward')
      }
      setSwapSeq((s) => s + 1)
      setTranslation(msg.id, { ...st, show: true, ready: false })
      return
    }

    const nextShow = !st.show
    setWanted(msg.id, nextShow)
    // 手動點回原文＝這則不要再自動翻回去；再點回譯文就解除。
    setDeclined(msg.id, !nextShow)
    if (!reduceMotion) {
      setAnim(nextShow ? 'forward' : 'backward')
      setSpin(nextShow ? 'forward' : 'backward')
    }
    setSwapSeq((s) => s + 1)
    setTranslation(msg.id, { ...st, show: nextShow, ready: false })
  }
  // 以前選了看譯文（localStorage 有記）→ 掛載時自動翻一次；載入中顯示原文＋「翻譯中…」。
  const restore = eligible && !off && st.status === 'idle' && isWanted(msg.id)
  // 自動翻新訊息：設定讀到了而且 auto 開著才動（讀不到＝不知道會不會花錢＝不動）。
  const fresh = eligible && !!ready?.ok && ready.auto && !!opts.recent && !msg.incomplete && st.status === 'idle' && !isDeclined(msg.id)
  const kind = restore ? 'restore' : fresh ? 'fresh' : null
  useEffect(() => {
    if (!kind) return
    if (kind === 'fresh' && !autoTranslateAllowed()) return
    if (claimAutoTranslate(msg.id)) run(kind)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kind, msg.id])
  if (!eligible) return { button: null, content: null, flip: null }

  const animClass =
    anim === 'forward'
      ? 'msg-swap-forward'
      : anim === 'backward'
        ? 'msg-swap-backward'
        : anim === 'auto'
          ? 'msg-swap-auto'
          : undefined

  const swapKey =
    anim !== null
      ? `${msg.id}-${st.status === 'done' && st.show ? 'tr' : 'orig'}-${swapSeq}`
      : `${msg.id}-${st.status === 'done' && st.show ? 'tr' : 'orig'}`

  return {
    button: createElement(MessageTranslationButton, {
      eligible,
      off,
      status: st.status,
      show: st.status === 'done' ? st.show : undefined,
      ready: st.status === 'done' ? st.ready : undefined,
      spin,
      onFlip: flipFace,
      onManual: () => { setWanted(msg.id, true); setDeclined(msg.id, false); run('manual') },
      onRedo: redo,
      redoing,
      rejectedReason: st.status === 'rejected' ? st.reason : undefined,
      rejectDetail: st.status === 'rejected' ? st.detail : undefined,
      onReport: () => {
        reportTranslation(msg.id).then(
          (r) => useStore.getState().notify('info', r.sent ? t('已回報給管理者') : t('10 分鐘內已回報過這則，不重複送出')),
          (e: unknown) => useStore.getState().notify('error', t('回報失敗：{reason}', { reason: e instanceof Error ? e.message : String(e) })),
        )
      },
      messageId: msg.id,
      onShowAnyway: st.status === 'rejected' && st.text
        ? () => { setWanted(msg.id, true); setTranslation(msg.id, { status: 'done', text: st.text!, show: true, ready: false, cost: null, cached: true, model: '', lowConfidence: true, jevProb: null }) }
        : undefined,
    }),
    content:
      st.status === 'done' && st.show
        ? createElement(MessageTranslationContent, {
            messageId: msg.id,
            text: st.text,
            botId: msg.bot_id,
            cost: st.cost,
            model: st.model,
            lowConfidence: st.lowConfidence,
            jevProb: st.jevProb,
          })
        : null,
    flip: st.status !== 'done' ? null : st.show ? 'tr' : 'orig',
    animClass,
    swapKey,
  }
}
