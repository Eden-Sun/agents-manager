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
          setTranslation(msg.id, { status: 'done', text: r.translation, show, ready: false, cost: r.costUsd, cached: r.cached, model: r.model, lowConfidence: r.lowConfidence, jevProb: r.jevProb })
          return
        }

        const hasSelection = typeof window !== 'undefined' && Boolean(window.getSelection()?.toString().trim())
        if (hasSelection) {
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
        if (e instanceof ApiError && e.status === 422 && e.body.error === 'translate_rejected') {
          const reason = typeof e.body.reason === 'string' ? e.body.reason : undefined
          const text = typeof e.body.translation === 'string' && e.body.translation.trim() ? e.body.translation : undefined
          const detail = isRejectDetail(e.body.detail) ? e.body.detail : undefined
          setTranslation(msg.id, { status: 'rejected', reason, text, detail })
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
        useStore.getState().notify('error', reason.includes('openai_not_configured')
          ? t('OpenAI 未啟用或未設定金鑰：翻譯的供應商設成 OpenAI，請啟用 OpenAI 並設定金鑰，或到「環境設定 → Claude API」把翻譯的供應商改回 Claude')
          : reason.includes('claude_api_not_configured')
          ? t('翻譯沒有開：到「環境設定 → Claude API」打開總開關與「對話訊息翻譯」，並確認 API key 已設定')
          : t('翻譯失敗：{reason}', { reason }))
      },
    )
  }
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
    setDeclined(msg.id, !nextShow)
    if (!reduceMotion) {
      setAnim(nextShow ? 'forward' : 'backward')
      setSpin(nextShow ? 'forward' : 'backward')
    }
    setSwapSeq((s) => s + 1)
    setTranslation(msg.id, { ...st, show: nextShow, ready: false })
  }
  const restore = eligible && !off && st.status === 'idle' && isWanted(msg.id)
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
