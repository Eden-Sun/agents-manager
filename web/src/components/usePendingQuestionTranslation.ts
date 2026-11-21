import { createElement, useEffect, useSyncExternalStore } from 'react'
import { translatePendingQuestions } from '../api'
import { isMostlyEnglish } from '../lib/isMostlyEnglish'
import { useT } from '../lib/i18n'
import type { PendingQuestion } from '../lib/pendingQuestion'
import { useStore } from '../store/store'
import { MessageTranslationButton } from './MessageTranslation'
import { resetTranslateReady, useTranslateReady } from './useTranslateReady'

const autoTried = new Set<string>()
const declined = new Set<string>()

export function resetPendingQuestionTranslation() {
  autoTried.clear()
  declined.clear()
  entries.clear()
  listeners.forEach((l) => l())
}

interface TextSet {
  header: string
  question: string
  options: { label: string; description: string }[]
}

type Entry =
  | { status: 'idle' | 'loading' | 'error'; key: string }
  | { status: 'done'; key: string; texts: TextSet[]; show: boolean }

const entries = new Map<string, Entry>()
const listeners = new Set<() => void>()
const IDLE: Entry = { status: 'idle', key: '' }
function setEntry(key: string, e: Entry) {
  entries.set(key, e)
  listeners.forEach((l) => l())
}
function subscribe(l: () => void) {
  listeners.add(l)
  return () => {
    listeners.delete(l)
  }
}

function payloadOf(questions: PendingQuestion[]) {
  return questions.map((q) => ({
    header: q.header,
    question: q.question,
    options: q.options.map((o) => ({ label: o.label, description: o.description })),
  }))
}

function textsOf(orig: PendingQuestion[], tr: Awaited<ReturnType<typeof translatePendingQuestions>>): TextSet[] {
  const byId = new Map(tr.questions.map((q) => [q.id, q]))
  return orig.map((q, qi) => {
    const t = byId.get(`q${qi}`)
    const optById = new Map((t?.options ?? []).map((o) => [o.id, o]))
    return {
      header: t?.header ?? '',
      question: t?.question ?? '',
      options: q.options.map((_, oi) => {
        const to = optById.get(`q${qi}-o${oi}`)
        return { label: to?.label ?? '', description: to?.description ?? '' }
      }),
    }
  })
}

function applyTexts(questions: PendingQuestion[], texts: TextSet[]): PendingQuestion[] {
  return questions.map((q, qi) => {
    const t = texts[qi]
    if (!t) return q
    const options = q.options.map((o, oi) => ({
      label: t.options[oi]?.label || o.label,
      description: t.options[oi]?.description || o.description,
    }))
    return {
      ...q,
      header: t.header || q.header,
      question: t.question || q.question,
      options,
      jev: q.jev ? { ...q.jev, label: options[q.jev.option]?.label ?? q.jev.label } : q.jev,
    }
  })
}

export function usePendingQuestionTranslation(questions: PendingQuestion[]): { button: React.ReactNode; display: PendingQuestion[] } {
  const t = useT()
  const payload = payloadOf(questions)
  const key = JSON.stringify(payload)
  const text = questions.map((q) => [q.header, q.question, ...q.options.flatMap((o) => [o.label, o.description])].join('\n')).join('\n')
  const eligible = questions.length > 0 && isMostlyEnglish(text)
  const ready = useTranslateReady(eligible)
  const off = ready && !ready.ok ? ready : null
  const snap = () => entries.get(key) ?? IDLE
  const cur = useSyncExternalStore(subscribe, snap, snap)

  const run = (kind: 'manual' | 'fresh') => {
    setEntry(key, { status: 'loading', key })
    translatePendingQuestions(payload).then(
      (r) => {
        const show = kind === 'manual' || !declined.has(key)
        setEntry(key, { status: 'done', key, texts: textsOf(questions, r), show })
      },
      (e: unknown) => {
        setEntry(key, { status: 'error', key })
        resetTranslateReady()
        if (kind !== 'manual') return
        const reason = e instanceof Error ? e.message : String(e)
        useStore.getState().notify('error', reason.includes('openai_not_configured')
          ? t('OpenAI 未啟用或未設定金鑰：翻譯的供應商設成 OpenAI，請啟用 OpenAI 並設定金鑰，或到「環境設定 → Claude API」把翻譯的供應商改回 Claude')
          : reason.includes('claude_api_not_configured')
          ? t('翻譯沒有開：到「環境設定 → Claude API」打開總開關與「對話訊息翻譯」，並確認 API key 已設定')
          : t('翻譯失敗：{reason}', { reason }))
      },
    )
  }

  const fresh = eligible && !!ready?.ok && ready.auto && cur.status === 'idle' && !declined.has(key) && !autoTried.has(key)
  useEffect(() => {
    if (!fresh) return
    if (autoTried.has(key) || declined.has(key) || (entries.get(key)?.status ?? 'idle') !== 'idle') return
    autoTried.add(key)
    run('fresh')
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fresh, key])

  const onFlip = () => {
    if (cur.status !== 'done') return
    if (cur.show) declined.add(key)
    else declined.delete(key)
    setEntry(key, { ...cur, show: !cur.show })
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
  const display = cur.status === 'done' && cur.show ? applyTexts(questions, cur.texts) : questions
  return { button, display }
}
