import { useState } from 'react'
import { translateMessage } from '../api'
import type { Message } from '../api/types'
import { isMostlyEnglish } from '../lib/isMostlyEnglish'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { SafeMarkdown } from './SafeMarkdown'

type State = { status: 'idle' | 'loading' | 'error' } | { status: 'done'; text: string; show: boolean }

/**
 * assistant 訊息說英文時，meta 列的「翻譯」鈕＋原文下方的譯文（daemon 用 Claude API 翻成繁中，同一則有快取不重複扣費）。
 * `button` 放 meta 列、`body` 放泡泡下面；不是英文就兩個都是 null。
 */
export function useMessageTranslation(msg: Message): { button: React.ReactNode; body: React.ReactNode } {
  const t = useT()
  const [st, setSt] = useState<State>({ status: 'idle' })
  const eligible = msg.role === 'assistant' && !msg.rewound_at && msg.source !== 'terminal_fallback' && isMostlyEnglish(msg.content)
  if (!eligible) return { button: null, body: null }

  const run = () => {
    setSt({ status: 'loading' })
    translateMessage(msg.id).then(
      (r) => setSt({ status: 'done', text: r.translation, show: true }),
      (e: unknown) => {
        setSt({ status: 'error' })
        useStore.getState().notify('error', t('翻譯失敗：{reason}', { reason: e instanceof Error ? e.message : String(e) }))
      },
    )
  }
  const onClick =
    st.status === 'done' ? () => setSt({ ...st, show: !st.show }) : st.status === 'loading' ? undefined : run
  const label = st.status === 'loading' ? t('翻譯中…') : st.status === 'done' ? (st.show ? t('隱藏譯文') : t('顯示譯文')) : t('翻譯')
  return {
    button: (
      <button type="button" className="msg-translate" disabled={st.status === 'loading'} onClick={onClick} title={t('用 Claude 翻成繁體中文')}>
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
