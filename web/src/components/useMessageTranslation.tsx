import { useState } from 'react'
import { translateMessage } from '../api'
import type { Message } from '../api/types'
import { isMostlyEnglish } from '../lib/isMostlyEnglish'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { SafeMarkdown } from './SafeMarkdown'
import { resetTranslateReady, useTranslateReady } from './useTranslateReady'

type State = { status: 'idle' | 'loading' | 'error' } | { status: 'done'; text: string; show: boolean }

/**
 * assistant 訊息說英文時，meta 列的「翻譯」鈕＋原文下方的譯文（daemon 用 Claude API 翻成繁中，同一則有快取不重複扣費）。
 * `button` 放 meta 列、`body` 放泡泡下面；不是英文就兩個都是 null。
 */
export function useMessageTranslation(msg: Message): { button: React.ReactNode; body: React.ReactNode } {
  const t = useT()
  const [st, setSt] = useState<State>({ status: 'idle' })
  const eligible = msg.role === 'assistant' && !msg.rewound_at && msg.source !== 'terminal_fallback' && isMostlyEnglish(msg.content)
  const ready = useTranslateReady(eligible)
  if (!eligible) return { button: null, body: null }
  const off = ready && !ready.ok ? ready : null

  const run = () => {
    setSt({ status: 'loading' })
    translateMessage(msg.id).then(
      (r) => setSt({ status: 'done', text: r.translation, show: true }),
      (e: unknown) => {
        setSt({ status: 'error' })
        resetTranslateReady()
        const reason = e instanceof Error ? e.message : String(e)
        // 未設定時直接講去哪裡開，不丟錯誤碼。
        useStore.getState().notify('error', reason.includes('claude_api_not_configured')
          ? t('翻譯沒有開：到「環境設定 → Claude API」打開總開關與「對話訊息翻譯」，並確認 API key 已設定')
          : t('翻譯失敗：{reason}', { reason }))
      },
    )
  }
  // 未啟用：按鈕照樣在（找得到這個功能），標「未啟用」，按了只提示去哪裡開，不打 API。
  const onClick = off
    ? () => useStore.getState().notify('info', t.tr(off.reason))
    : st.status === 'done' ? () => setSt({ ...st, show: !st.show }) : st.status === 'loading' ? undefined : run
  const label = off ? t('翻譯（未啟用）') : st.status === 'loading' ? t('翻譯中…') : st.status === 'done' ? (st.show ? t('隱藏譯文') : t('顯示譯文')) : t('翻譯')
  return {
    button: (
      <button type="button" className={`msg-translate${off ? ' off' : ''}`} disabled={st.status === 'loading'} onClick={onClick} title={off ? t.tr(off.reason) : t('用 Claude 翻成繁體中文')}>
        {label}
      </button>
    ),
    body:
      st.status === 'done' && st.show ? (
        <div className="bubble md msg-translation" lang="zh-Hant">
          <SafeMarkdown text={st.text} botId={msg.bot_id} />
        </div>
      ) : null,
  }
}
