import type { PendingQuestion } from '../lib/pendingQuestion'
import './pendingQuestionCard.css'

/**
 * 畫面上看不到題目時（pane 太矮、claude 把選單裁掉），把 transcript 裡的題目與選項列出來（2026-09-23 使用者）。
 * 只讀：怎麼作答照舊用下面的按鍵（編號對應下面這份清單的順序）。
 */
export function PendingQuestionCard({ questions }: { questions: PendingQuestion[] }) {
  if (questions.length === 0) return null
  return (
    <div className="pending-q" role="note" aria-label="agent 在問的問題">
      <p className="pending-q-src">終端太矮，題目被截掉了——下面是從對話紀錄讀出來的原題：</p>
      {questions.map((q, qi) => (
        <div key={`${qi}-${q.question}`} className="pending-q-item">
          {q.header ? <span className="pending-q-header">{q.header}</span> : null}
          <p className="pending-q-text">{q.question}</p>
          {q.options.length ? (
            <ol className="pending-q-opts">
              {q.options.map((o) => (
                <li key={o.label}>
                  <span className="pending-q-label">{o.label}</span>
                  {o.description ? <span className="pending-q-desc">{o.description}</span> : null}
                </li>
              ))}
            </ol>
          ) : null}
          {q.multiSelect ? <p className="pending-q-note">可複選</p> : null}
        </div>
      ))}
    </div>
  )
}
