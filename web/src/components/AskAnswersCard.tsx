import type { Message } from '../api/types'
import { useStore } from '../store/store'
import { useTapCopy } from '../hooks/useTapCopy'
import { NOT_ANSWERED, askAnswersText } from '../lib/askAnswers'
import { msgTimeLabel, msgTimeTitle } from '../lib/msgTime'
import { useLocalDay } from '../hooks/useLocalDay'
import type { AskAnswers } from '../lib/askAnswers'
import { useT } from '../lib/i18n'
import './askAnswers.css'

/**
 * 「Claude 問／你答」：claude 的 `AskUserQuestion` 與使用者的回答（網頁或終端答的都一樣）。
 * 只讀的一張卡：不是使用者打的 prompt，沒有倒回鍵、來源標、送達標，也不碰任何送出或排隊。
 */
export function AskAnswersCard({ msg, ask }: { msg: Message; ask: AskAnswers }) {
  const t = useT()
  const today = useLocalDay()
  const notify = useStore((s) => s.notify)
  const tapCopy = useTapCopy(askAnswersText(ask), (ok) => {
    notify(ok ? 'info' : 'error', ok ? t('已複製問答') : t('複製失敗，請長按選取文字複製'))
  })
  return (
    <article className="msg ask-answers" data-msg-id={msg.id} aria-label={t('Claude 的提問與你的回答')}>
      <div className="msg-meta msg-meta-above">
        <div className="msg-meta-left">
          <span className="src-tag mono" title={t('Claude 用 AskUserQuestion 問、你回答的紀錄；不是你打的字')}>
            {ask.answered ? t('提問與回答') : t('提問（沒有回答）')}
          </span>
        </div>
        <time className="msg-time" dateTime={msg.created_at} title={msgTimeTitle(msg.created_at)}>
          {msgTimeLabel(msg.created_at, today)}
        </time>
      </div>
      <div className="ask-card bubble-copyable" {...tapCopy}>
        {ask.items.map((i, n) => (
          <section className="ask-item" key={`${n}:${i.question}`}>
            <div className="ask-q">
              <span className="ask-who">{t('Claude 問')}</span>
              {i.header ? <span className="ask-header">{i.header}</span> : null}
              <span className="ask-text">{i.question}</span>
            </div>
            <div className={`ask-a${i.answer === null ? ' none' : ''}`}>
              <span className="ask-who">{t('你答')}</span>
              <span className="ask-text">{i.answer ?? NOT_ANSWERED}</span>
            </div>
            {i.notes ? <div className="ask-notes">{t('備註：{notes}', { notes: i.notes })}</div> : null}
          </section>
        ))}
      </div>
    </article>
  )
}
