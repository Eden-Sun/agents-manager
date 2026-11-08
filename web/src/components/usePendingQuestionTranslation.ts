import { createElement, useEffect, useState } from 'react'
import { translatePendingQuestions } from '../api'
import { isMostlyEnglish } from '../lib/isMostlyEnglish'
import { useT } from '../lib/i18n'
import type { PendingQuestion } from '../lib/pendingQuestion'
import { useStore } from '../store/store'
import { MessageTranslationButton } from './MessageTranslation'
import { resetTranslateReady, useTranslateReady } from './useTranslateReady'

/** 這次頁面生命週期已自動試過的問卷（鍵＝送出去的內容）：失敗不一直重試，重新掛載也不重打。 */
const autoTried = new Set<string>()
/** 使用者手動點回原文的問卷：自動翻譯不再翻回去，再點「翻譯」才解除（同訊息翻譯）。 */
const declined = new Set<string>()

/** 給測試：清掉本頁的「試過／點回原文」記憶。 */
export function resetPendingQuestionTranslation() {
  autoTried.clear()
  declined.clear()
}

type Entry =
  | { status: 'idle' | 'loading' | 'error'; key: string }
  | { status: 'done'; key: string; items: PendingQuestion[]; show: boolean }

/** 送去翻的內容：只含要翻的文字，順序與 index 跟 `questions` 一致，daemon 依位置產生 id。 */
function payloadOf(questions: PendingQuestion[]) {
  return questions.map((q) => ({
    header: q.header,
    question: q.question,
    options: q.options.map((o) => ({ label: o.label, description: o.description })),
  }))
}

/** 把譯文套回題目：只換顯示的字；選項的位置（數字鍵）、label 原值（送回 CLI 的）都不動。 */
function applyTranslation(orig: PendingQuestion[], tr: Awaited<ReturnType<typeof translatePendingQuestions>>): PendingQuestion[] {
  const byId = new Map(tr.questions.map((q) => [q.id, q]))
  return orig.map((q, qi) => {
    const t = byId.get(`q${qi}`)
    if (!t) return q
    const optById = new Map(t.options.map((o) => [o.id, o]))
    const options = q.options.map((o, oi) => {
      const to = optById.get(`q${qi}-o${oi}`)
      return { label: to?.label || o.label, description: to ? to.description : o.description }
    })
    return {
      ...q,
      header: t.header || q.header,
      question: t.question || q.question,
      options,
      // Jev 的建議只顯示譯後的 label；`option` 是原位置，不變。
      jev: q.jev ? { ...q.jev, label: options[q.jev.option]?.label ?? q.jev.label } : q.jev,
    }
  })
}

/**
 * 待答問卷（「需要回應」面板的原題卡）的譯文：題目與選項主要是英文時，跟對話訊息一樣有「翻譯」鈕。
 * auto 開著、用途啟用有 key 時新掛載就自動翻一次（daemon 有快取，同一份不重扣費）；`display` 是要顯示的題目（譯文或原文）。
 * 只影響顯示：呼叫端的 `questions`（作答對得上畫面、送鍵用的）不換。
 */
export function usePendingQuestionTranslation(questions: PendingQuestion[]): { button: React.ReactNode; display: PendingQuestion[] } {
  const t = useT()
  const payload = payloadOf(questions)
  const key = JSON.stringify(payload)
  const text = questions.map((q) => [q.header, q.question, ...q.options.flatMap((o) => [o.label, o.description])].join('\n')).join('\n')
  const eligible = questions.length > 0 && isMostlyEnglish(text)
  const ready = useTranslateReady(eligible)
  const off = ready && !ready.ok ? ready : null
  const [entry, setEntry] = useState<Entry>({ status: 'idle', key })
  // 內容變了（新題目）就當作還沒翻過；舊的結果不沿用。
  const cur: Entry = entry.key === key ? entry : { status: 'idle', key }

  const run = (kind: 'manual' | 'fresh') => {
    setEntry({ status: 'loading', key })
    translatePendingQuestions(payload).then(
      (r) => {
        const show = kind === 'manual' || !declined.has(key)
        setEntry({ status: 'done', key, items: applyTranslation(questions, r), show })
      },
      (e: unknown) => {
        setEntry({ status: 'error', key })
        resetTranslateReady()
        if (kind !== 'manual') return
        const reason = e instanceof Error ? e.message : String(e)
        useStore.getState().notify('error', reason.includes('claude_api_not_configured')
          ? t('翻譯沒有開：到「環境設定 → Claude API」打開總開關與「對話訊息翻譯」，並確認 API key 已設定')
          : t('翻譯失敗：{reason}', { reason }))
      },
    )
  }

  // 自動翻：設定讀到、auto 開著、還沒試過這份問卷、使用者沒手動點回原文。
  const fresh = eligible && !!ready?.ok && ready.auto && cur.status === 'idle' && !declined.has(key) && !autoTried.has(key)
  useEffect(() => {
    if (!fresh) return
    autoTried.add(key)
    run('fresh')
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fresh, key])

  const onFlip = () => {
    if (cur.status !== 'done') return
    if (cur.show) declined.add(key)
    else declined.delete(key)
    setEntry({ ...cur, show: !cur.show })
  }

  const button = eligible
    ? createElement(MessageTranslationButton, {
        eligible,
        off,
        status: cur.status,
        show: cur.status === 'done' ? cur.show : undefined,
        onFlip,
        onManual: () => {
          declined.delete(key)
          run('manual')
        },
      })
    : null
  const display = cur.status === 'done' && cur.show ? cur.items : questions
  return { button, display }
}
