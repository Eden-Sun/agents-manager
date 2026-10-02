import type { Message } from '../api/types'
import { useStore } from '../store/store'
import { useTapCopy } from '../hooks/useTapCopy'
import { NOT_ANSWERED, askAnswersText } from '../lib/askAnswers'
import type { AskAnswers } from '../lib/askAnswers'
import './askAnswers.css'

function timeOf(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? '' : d.toLocaleTimeString([], { hour12: false, hour: '2-digit', minute: '2-digit' })
}

/**
 * 「Claude 問／你答」：claude 的 `AskUserQuestion` 與使用者的回答（網頁或終端答的都一樣）。
 * 只讀的一張卡：不是使用者打的 prompt，沒有倒回鍵、來源標、送達標，也不碰任何送出或排隊。
 */
export function AskAnswersCard({ msg, ask }: { msg: Message; ask: AskAnswers }) {
  const notify = useStore((s) => s.notify)
  const tapCopy = useTapCopy(askAnswersText(ask), (ok) => {
    notify(ok ? 'info' : 'error', ok ? '已複製問答' : '複製失敗，請長按選取文字複製')
  })
  return (
    <article className="msg ask-answers" data-msg-id={msg.id} aria-label="Claude 的提問與你的回答">
      <div className="msg-meta msg-meta-above">
        <div className="msg-meta-left">
          <span className="src-tag mono" title="Claude 用 AskUserQuestion 問、你回答的紀錄；不是你打的字">
            {ask.answered ? '提問與回答' : '提問（沒有回答）'}
          </span>
        </div>
        <time className="msg-time" dateTime={msg.created_at} title={msg.created_at}>
          {timeOf(msg.created_at)}
        </time>
      </div>
      <div className="ask-card bubble-copyable" {...tapCopy}>
        {ask.items.map((i, n) => (
          <section className="ask-item" key={`${n}:${i.question}`}>
            <div className="ask-q">
              <span className="ask-who">Claude 問</span>
              {i.header ? <span className="ask-header">{i.header}</span> : null}
              <span className="ask-text">{i.question}</span>
            </div>
            <div className={`ask-a${i.answer === null ? ' none' : ''}`}>
              <span className="ask-who">你答</span>
              <span className="ask-text">{i.answer ?? NOT_ANSWERED}</span>
            </div>
            {i.notes ? <div className="ask-notes">備註：{i.notes}</div> : null}
          </section>
        ))}
      </div>
    </article>
  )
}
