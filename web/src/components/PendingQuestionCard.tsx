import { useState } from 'react'
import { JEV_ASK_NOTE, type PendingQuestion } from '../lib/pendingQuestion'
import { JevLabel } from './JevLabel'
import './pendingQuestionCard.css'

/**
 * 畫面上看不到題目時（pane 太矮、claude 把選單裁掉），把 transcript 裡的題目與選項列出來（2026-09-23 使用者）。
 *
 * **唯讀摘要，不是選項區**（issue #559）：作答一律走畫面上認出來的那份選單（`BlockedChoices`）——送鍵前要重讀畫面、
 * 對得上才送，游標在哪、有沒有 `Type something`／`Submit` 列都只有畫面知道，transcript 不知道。所以這裡的選項
 * 不編號、不加粗、不畫框，畫面上正在答的那一題不再列一次選項（`current`）。
 * 認得出選單時預設收成一行（`defaultOpen=false`）：2026-09-25 使用者手機上整張攤開把選項擠到只剩半列，點不到。
 */
export function PendingQuestionCard({
  questions,
  current = -1,
  defaultOpen = false,
  answerWhere = 'above',
}: {
  questions: PendingQuestion[]
  /** 畫面上正在答的是第幾題（`pendingIndexOnScreen`）；-1 ＝對不上。 */
  current?: number
  defaultOpen?: boolean
  /** 作答的地方在這張卡的上面（選單模式）還是下面（終端快照＋按鍵）。 */
  answerWhere?: 'above' | 'below'
}) {
  const [open, setOpen] = useState(defaultOpen)
  if (questions.length === 0) return null
  // Jev 的選項建議（issue #1475）：只提示、不代答；沒有建議（沒開、失敗、分數太接近、多選題）就什麼都不顯示。
  const jevPicks = questions.filter((q) => q.jev)
  const anyJev = jevPicks.length > 0
  return (
    <div className={`pending-q${open ? ' pending-q-open' : ''}`} role="note" aria-label="agent 在問的問題（對話紀錄裡的原題）">
      <button type="button" className="pending-q-toggle" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <span aria-hidden="true">{open ? '▾' : '▸'}</span>
        原題（{questions.length} 題）
        <span className="pending-q-src">終端沒畫出全部題目，從對話紀錄讀出</span>
        {anyJev ? (
          <span className="pending-q-jev">
            <JevLabel prob={jevPicks.length === 1 ? jevPicks[0].jev?.score : null} />
            {jevPicks.length > 1 ? `（${jevPicks.length} 題）` : null}
          </span>
        ) : null}
      </button>
      {open ? (
        <>
          <p className="pending-q-note">只供閱讀；作答用{answerWhere === 'above' ? '上面的選項' : '下面的按鍵'}。</p>
          {anyJev ? <p className="pending-q-jev-note">{JEV_ASK_NOTE}</p> : null}
          {questions.map((q, qi) => (
            <div key={`${qi}-${q.question}`} className={`pending-q-item${qi === current ? ' pending-q-current' : ''}`}>
              <p className="pending-q-meta">
                {questions.length > 1 ? `第 ${qi + 1} 題` : null}
                {q.header ? <span className="pending-q-header">{q.header}</span> : null}
                {q.multiSelect ? <span>可複選</span> : null}
                {qi === current ? <span className="pending-q-here">畫面上這一題</span> : null}
              </p>
              <p className="pending-q-text">{q.question}</p>
              {qi === current && answerWhere === 'above' ? (
                // 選項在上面的選單裡、這裡不再列：建議改成一行說明，不然選單與這張卡都看不到它。
                q.jev ? (
                  <p className="pending-q-jev-line">
                    <JevLabel className="pending-q-jev" prob={q.jev.score} />
                    {q.jev.label}
                  </p>
                ) : null
              ) : q.options.length ? (
                <ul className="pending-q-opts">
                  {q.options.map((o, oi) => (
                    <li key={o.label}>
                      {o.label}
                      {o.description ? <span className="pending-q-desc">：{o.description}</span> : null}
                      {q.jev && q.jev.option === oi ? <JevLabel className="pending-q-jev" prob={q.jev.score} /> : null}
                    </li>
                  ))}
                </ul>
              ) : null}
            </div>
          ))}
        </>
      ) : null}
    </div>
  )
}
