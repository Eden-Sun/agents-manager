import { createElement, useEffect, useSyncExternalStore } from 'react'
import { translateMessage } from '../api'
import type { Message } from '../api/types'
import { isMostlyEnglish } from '../lib/isMostlyEnglish'
import { useT } from '../lib/i18n'
import { autoTranslateAllowed, claimAutoTranslate, getTranslation, isDeclined, isWanted, noteAutoFailure, setDeclined, setTranslation, setWanted, subscribeTranslations } from '../lib/messageTranslationStore'
import { useStore } from '../store/store'
import { resetTranslateReady, useTranslateReady } from './useTranslateReady'
import { MessageTranslationButton, MessageTranslationContent } from './MessageTranslation'
import { AUTO_TRANSLATE_RECENT, usd } from './messageTranslationConstants'

export { AUTO_TRANSLATE_RECENT, usd }

/**
 * assistant 訊息說英文時，meta 列的「翻譯」鈕＋譯文（daemon 用 Claude API 翻成繁中，同一則有快取不重複扣費）。
 * 譯文跟原文在**同一個泡泡**裡翻面切換（使用者 2026-11-04：不要在下面多一塊，要像翻面）：`content` 不是 null 時，
 * 呼叫端用它取代泡泡裡的原文。
 * 自動翻譯（使用者 2026-11-06）：daemon 設定 `[claude_api.uses.message_translate] auto` 開著、用途啟用有 key、`recent`（呼叫端說這則在對話最後 N 則內）、
 * 訊息已完整（`!incomplete`，串流中不打 API）、使用者沒手動點回過原文時，掛載就自動翻並顯示譯文。同一則本頁只自動試一次，失敗停在可手動重試的狀態。
 * `flip` 沒翻過是 null，翻過後是正在看的那一面（tr／orig），呼叫端拿來當 key 重播翻面動畫。不是英文就全是 null。
 */
export function useMessageTranslation(msg: Message, opts: { recent?: boolean } = {}): { button: React.ReactNode; content: React.ReactNode; flip: 'tr' | 'orig' | null } {
  const t = useT()
  const st = useSyncExternalStore(subscribeTranslations, () => getTranslation(msg.id), () => getTranslation(msg.id))
  const eligible = msg.role === 'assistant' && !msg.rewound_at && msg.source !== 'terminal_fallback' && isMostlyEnglish(msg.content)
  const ready = useTranslateReady(eligible)
  const off = ready && !ready.ok ? ready : null

  // 狀態在 store（以 msg.id 為鍵）：remount 不會掉回原文。
  // manual＝使用者按的；restore＝重整後重抓以前選過看譯文的；fresh＝自動翻譯新訊息。後兩者失敗不跳錯誤也不重試。
  const run = (kind: 'manual' | 'restore' | 'fresh') => {
    setTranslation(msg.id, { status: 'loading' })
    translateMessage(msg.id).then(
      (r) => {
        const show = kind === 'manual' || (kind === 'restore' ? isWanted(msg.id) : !isDeclined(msg.id))
        // 自動翻出來並顯示的，跟手動點過一樣記著：之後滑出最後 N 則、重整，都維持譯文。
        if (kind === 'fresh' && show) setWanted(msg.id, true)
        setTranslation(msg.id, { status: 'done', text: r.translation, show, cost: r.costUsd, cached: r.cached, model: r.model })
      },
      (e: unknown) => {
        setTranslation(msg.id, { status: 'error' })
        resetTranslateReady()
        if (kind !== 'manual') {
          if (kind === 'fresh') noteAutoFailure()
          return
        }
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
    // 手動點回原文＝這則不要再自動翻回去；再點回譯文就解除。
    setDeclined(msg.id, st.show)
    setTranslation(msg.id, { ...st, show: !st.show })
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

  return {
    button: createElement(MessageTranslationButton, {
      eligible,
      off,
      status: st.status,
      show: st.status === 'done' ? st.show : undefined,
      onFlip: flipFace,
      onManual: () => { setWanted(msg.id, true); setDeclined(msg.id, false); run('manual') },
    }),
    content:
      st.status === 'done' && st.show
        ? createElement(MessageTranslationContent, {
            text: st.text,
            botId: msg.bot_id,
            cost: st.cost,
            cached: st.cached,
            model: st.model,
          })
        : null,
    flip: st.status !== 'done' ? null : st.show ? 'tr' : 'orig',
  }
}
