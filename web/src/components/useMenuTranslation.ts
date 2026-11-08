import type { PendingQuestion } from '../lib/pendingQuestion'
import { usePendingQuestionTranslation } from './usePendingQuestionTranslation'

/** daemon 一次最多收這麼多題（`PQ_MAX_QUESTIONS`）；超過就不翻，留原文。 */
const MAX_PAGES = 8

/** 終端畫面上解析出來的一頁問卷（`TuiChoiceMenu` 或多分頁的一格）：只取要翻的文字。 */
export interface MenuPageText {
  /** 分頁列的名字（header chip）。 */
  header: string
  question: string | null
  multi: boolean
  choices: { title: string; detail: string }[]
}

/** 一頁的譯文；沒有譯文的欄位是 `null`，呼叫端退回原文。 */
export interface MenuPageDisplay {
  header: string | null
  question: string | null
  titles: (string | null)[]
  details: (string | null)[]
}

function toQuestion(p: MenuPageText): PendingQuestion {
  return {
    header: p.header,
    question: p.question ?? '',
    multiSelect: p.multi,
    options: p.choices.map((c) => ({ label: c.title, description: c.detail })),
    jev: null,
  }
}

/**
 * 終端問卷（即時選單或多分頁預載）的譯文：沿用原題卡的 `usePendingQuestionTranslation`（同一支 API、同一套自動翻／切換／失敗重試），
 * 所以內容一樣時 daemon 的快取直接共用。只換顯示；作答、送鍵、比對一律用呼叫端的原文。
 */
export function useMenuTranslation(pages: MenuPageText[]): {
  button: React.ReactNode
  page: (k: number) => MenuPageDisplay | null
} {
  const usable = pages.length > 0 && pages.length <= MAX_PAGES ? pages : []
  const { button, display } = usePendingQuestionTranslation(usable.map(toQuestion))
  const page = (k: number): MenuPageDisplay | null => {
    const d = display[k]
    const p = usable[k]
    if (!d || !p) return null
    // `display` 不翻時就是原文；譯文是空字串就退回原文（`null`），不讓空白蓋掉畫面。
    return {
      header: d.header || null,
      question: d.question || null,
      titles: p.choices.map((_, j) => d.options[j]?.label || null),
      details: p.choices.map((_, j) => d.options[j]?.description || null),
    }
  }
  return { button, page }
}
