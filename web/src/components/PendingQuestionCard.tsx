import { useT } from '../lib/i18n'
import { useState } from 'react'
import { JEV_ASK_NOTE, type PendingQuestion } from '../lib/pendingQuestion'
import { JevLabel } from './JevLabel'
import { usePendingQuestionTranslation } from './usePendingQuestionTranslation'
import './pendingQuestionCard.css'

export function PendingQuestionCard({

  questions,
  current = -1,
  defaultOpen = false,
  answerWhere = 'above',
}: {
  questions: PendingQuestion[]
  current?: number
  defaultOpen?: boolean
  answerWhere?: 'above' | 'below'
}) {
  const [open, setOpen] = useState(defaultOpen)
  const t = useT()
  const tr = usePendingQuestionTranslation(questions)
  if (questions.length === 0) return null
  const shown = tr.display
  const jevPicks = questions.filter((q) => q.jev)
  const anyJev = jevPicks.length > 0
  return (
    <div className={`pending-q${open ? ' pending-q-open' : ''}`} role="note" aria-label={t("agent 在問的問題（對話紀錄裡的原題）")}>
      <button type="button" className="pending-q-toggle" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <span aria-hidden="true">{open ? '▾' : '▸'}</span>
        {t('原題（{n} 題）', { n: questions.length })}
        <span className="pending-q-src">{t("終端沒畫出全部題目，從對話紀錄讀出")}</span>
        {anyJev ? (
          <span className="pending-q-jev">
            <JevLabel prob={jevPicks.length === 1 ? jevPicks[0].jev?.score : null} />
            {jevPicks.length > 1 ? t('（{n} 題）', { n: jevPicks.length }) : null}
          </span>
        ) : null}
      </button>
      {open ? (
        <>
          {tr.button ? <div className="pending-q-tr">{tr.button}</div> : null}
          <p className="pending-q-note">{answerWhere === 'above' ? t('只供閱讀；作答用上面的選項。') : t('只供閱讀；作答用下面的按鍵。')}</p>
          {anyJev ? <p className="pending-q-jev-note">{t.tr(JEV_ASK_NOTE)}</p> : null}
          {shown.map((q, qi) => (
            <div key={`${qi}-${q.question}`} className={`pending-q-item${qi === current ? ' pending-q-current' : ''}`}>
              <p className="pending-q-meta">
                {questions.length > 1 ? t('第 {n} 題', { n: qi + 1 }) : null}
                {q.header ? <span className="pending-q-header">{q.header}</span> : null}
                {q.multiSelect ? <span>{t('可複選')}</span> : null}
                {qi === current ? <span className="pending-q-here">{t('畫面上這一題')}</span> : null}
              </p>
              <p className="pending-q-text">{q.question}</p>
              {qi === current && answerWhere === 'above' ? (
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
                      {o.description ? <span className="pending-q-desc">{t('：{desc}', { desc: o.description })}</span> : null}
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
