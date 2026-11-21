import { fmtDateTime } from '../lib/userTime'
import { useEffect, useState } from 'react'
import { fetchJudgeShadow, type JudgeShadow } from '../api/judge'
import { tr, useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { JevLabel } from './JevLabel'
import { botLabel, JUDGE_SCENES, JUDGE_WINDOWS, overThreshold, probabilityText, recentRows, sceneStats, type JudgeWindow } from '../lib/judgeStats'

const LEDGER_LIMIT = 200

type LedgerLoad = { kind: 'loading' } | { kind: 'unsupported' } | { kind: 'error'; message: string } | { kind: 'ok'; shadow: JudgeShadow }

const sceneName = (id: string) => {
  const name = JUDGE_SCENES.find((s) => s.id === id)?.name
  return name === undefined ? id : tr(name)
}

function when(at: string): string {
  const t = Date.parse(at)
  return Number.isNaN(t) ? at : fmtDateTime(new Date(t), { hour12: false })
}

export function JudgeLedger() {
  const t = useT()
  const bots = useStore((s) => s.bots)
  const [load, setLoad] = useState<LedgerLoad>({ kind: 'loading' })
  const [reload, setReload] = useState(0)

  useEffect(() => {
    let alive = true
    fetchJudgeShadow(LEDGER_LIMIT)
      .then((shadow) => {
        if (!alive) return
        setLoad(shadow ? { kind: 'ok', shadow } : { kind: 'unsupported' })
      })
      .catch((e: unknown) => {
        if (!alive) return
        setLoad({ kind: 'error', message: e instanceof Error ? e.message : String(e) })
      })
    return () => {
      alive = false
    }
  }, [reload])

  return (
    <section className="judge-ledger" aria-label={t('Jev 帳本')}>
      <h4>{t('帳本')}</h4>
      <p className="hint">
        <JevLabel note />
        <br />
        {t('這裡每一筆都是 Jev（TypeSafe 型別判斷）給的判斷，「Jev 機率」是它給的分數。')}
      </p>
      {load.kind === 'loading' ? <p className="hint">{t('讀取中…')}</p> : null}
      {load.kind === 'unsupported' ? <p className="hint">{t('這顆 daemon 還沒有撞限第二意見帳本（需要更新 daemon）。')}</p> : null}
      {load.kind === 'error' ? (
        <p className="hint">
          {t('讀不到帳本：{message}', { message: load.message })}{' '}
          <button
            type="button"
            className="btn"
            onClick={() => {
              setLoad({ kind: 'loading' })
              setReload((n) => n + 1)
            }}
          >
            {t('重試')}
          </button>
        </p>
      ) : null}
      {load.kind === 'ok' ? <LedgerBody shadow={load.shadow} bots={bots} /> : null}
    </section>
  )
}

function LedgerBody({ shadow, bots }: { shadow: JudgeShadow; bots: { id: string; name: string }[] }) {
  const t = useT()
  if (shadow.rows.length === 0) {
    return (
      <>
        <p className="hint">{t('還沒被觸發過。各場景的觸發條件：')}</p>
        <ul className="judge-ledger-triggers">
          {JUDGE_SCENES.map((s) => (
            <li key={s.id}>
              <b>{t.tr(s.name)}</b>
              {t('：')}
              {t.tr(s.when)}
            </li>
          ))}
        </ul>
      </>
    )
  }
  const stats = sceneStats(shadow.rows)
  const recent = recentRows(shadow.rows, 20)
  return (
    <>
      {shadow.rows.length >= LEDGER_LIMIT ? (
        <p className="hint">{t('只統計最近 {n} 筆：「全部」與「近 7 天」可能少算更早的紀錄。', { n: LEDGER_LIMIT })}</p>
      ) : null}
      <div className="judge-ledger-scroll">
        <table className="judge-ledger-table" role="table">
          <thead role="rowgroup">
            <tr role="row">
              <th role="columnheader">{t('場景')}</th>
              {JUDGE_WINDOWS.map((w) => (
                <th key={w.id} role="columnheader">{t.tr(w.label)}</th>
              ))}
              <th role="columnheader">{t('最後一次')}</th>
            </tr>
          </thead>
          <tbody role="rowgroup">
            {stats.map((st) => (
              <tr key={st.scene} role="row">
                <td role="cell" data-label={t('場景')} className="scene">
                  <b>{sceneName(st.scene)}</b>
                  <div className="hint">{t.tr(JUDGE_SCENES.find((s) => s.id === st.scene)?.when ?? '')}</div>
                </td>
                {JUDGE_WINDOWS.map((w) => {
                  const c = st.windows[w.id as JudgeWindow]
                  return (
                    <td key={w.id} role="cell" data-label={t.tr(w.label)} className="num">
                      {t('{n} 筆', { n: c.total })}
                      <div className="hint">{t('超過門檻 {over}・錯誤 {errors}', { over: c.over === null ? '—' : c.over, errors: c.errors })}</div>
                    </td>
                  )
                })}
                <td role="cell" data-label={t('最後一次')} className="num">
                  {st.last ? when(st.last) : '—'}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <details className="env-fold">
        <summary>{t('最近 20 筆')}</summary>
        <div className="judge-ledger-scroll">
          <table className="judge-ledger-table" role="table">
            <thead role="rowgroup">
              <tr role="row">
                <th role="columnheader">{t('時間')}</th>
                <th role="columnheader">{t('場景')}</th>
                <th role="columnheader">bot</th>
                <th role="columnheader">{t('Jev 機率')}</th>
                <th role="columnheader">{t('耗時 ms')}</th>
                <th role="columnheader">{t('錯誤')}</th>
              </tr>
            </thead>
            <tbody role="rowgroup">
              {recent.map((r) => (
                <tr key={r.id} role="row" className={overThreshold(r) === true ? 'over' : undefined}>
                  <td role="cell" data-label={t('時間')} className="num">
                    {when(r.at)}
                  </td>
                  <td role="cell" data-label={t('場景')}>{sceneName(r.regex_verdict)}</td>
                  <td role="cell" data-label="bot">{botLabel(r.bot_id, bots)}</td>
                  <td role="cell" data-label={t('Jev 機率')} className="num">
                    {probabilityText(r)}
                  </td>
                  <td role="cell" data-label={t('耗時 ms')} className="num">
                    {r.ms ?? '—'}
                  </td>
                  <td role="cell" data-label={t('錯誤')}>{r.error ? r.error : '—'}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </details>
    </>
  )
}
