/**
 * 環境設定 → Jev 建議 底下的「帳本」（SPEC §4.3c，issue #1327）：四個場景各一列的計數，加最近 20 筆。
 * 只讀；不顯示遮罩後的命中行（那是給 daemon 對帳用的）。舊 daemon 沒有這個端點 → 說明，不報錯。
 */
import { useEffect, useState } from 'react'
import { fetchJudgeShadow, type JudgeShadow } from '../api/judge'
import { useStore } from '../store/store'
import { JevLabel } from './JevLabel'
import { botLabel, JUDGE_SCENES, JUDGE_WINDOWS, overThreshold, probabilityText, recentRows, sceneStats, type JudgeWindow } from '../lib/judgeStats'

/** 帳本一次拿的筆數（`fetchJudgeShadow` 預設值）；到了這個數字，統計只算得到這麼多筆。 */
const LEDGER_LIMIT = 200

type LedgerLoad = { kind: 'loading' } | { kind: 'unsupported' } | { kind: 'error'; message: string } | { kind: 'ok'; shadow: JudgeShadow }

const sceneName = (id: string) => JUDGE_SCENES.find((s) => s.id === id)?.name ?? id

function when(at: string): string {
  const t = Date.parse(at)
  return Number.isNaN(t) ? at : new Date(t).toLocaleString('zh-TW', { hour12: false })
}

export function JudgeLedger() {
  const bots = useStore((s) => s.bots)
  const [load, setLoad] = useState<LedgerLoad>({ kind: 'loading' })
  /** 按「重試」就 +1，讓下面那個 effect 重跑一次。 */
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
    <section className="judge-ledger" aria-label="Jev 帳本">
      <h4>帳本</h4>
      <p className="hint">
        <JevLabel note />
        <br />
        這裡每一筆都是 Jev（TypeSafe 型別判斷）給的判斷，「Jev 機率」是它給的分數。
      </p>
      {load.kind === 'loading' ? <p className="hint">讀取中…</p> : null}
      {load.kind === 'unsupported' ? <p className="hint">這顆 daemon 還沒有撞限第二意見帳本（需要更新 daemon）。</p> : null}
      {load.kind === 'error' ? (
        <p className="hint">
          讀不到帳本：{load.message}{' '}
          <button
            type="button"
            className="btn"
            onClick={() => {
              setLoad({ kind: 'loading' })
              setReload((n) => n + 1)
            }}
          >
            重試
          </button>
        </p>
      ) : null}
      {load.kind === 'ok' ? <LedgerBody shadow={load.shadow} bots={bots} /> : null}
    </section>
  )
}

function LedgerBody({ shadow, bots }: { shadow: JudgeShadow; bots: { id: string; name: string }[] }) {
  if (shadow.rows.length === 0) {
    return (
      <>
        <p className="hint">還沒被觸發過。各場景的觸發條件：</p>
        <ul className="judge-ledger-triggers">
          {JUDGE_SCENES.map((s) => (
            <li key={s.id}>
              <b>{s.name}</b>：{s.when}
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
        <p className="hint">只統計最近 {LEDGER_LIMIT} 筆：「全部」與「近 7 天」可能少算更早的紀錄。</p>
      ) : null}
      <div className="judge-ledger-scroll">
        <table className="judge-ledger-table" role="table">
          <thead role="rowgroup">
            <tr role="row">
              <th role="columnheader">場景</th>
              {JUDGE_WINDOWS.map((w) => (
                <th key={w.id} role="columnheader">{w.label}</th>
              ))}
              <th role="columnheader">最後一次</th>
            </tr>
          </thead>
          <tbody role="rowgroup">
            {stats.map((st) => (
              <tr key={st.scene} role="row">
                <td role="cell" data-label="場景" className="scene">
                  <b>{sceneName(st.scene)}</b>
                  <div className="hint">{JUDGE_SCENES.find((s) => s.id === st.scene)?.when}</div>
                </td>
                {JUDGE_WINDOWS.map((w) => {
                  const c = st.windows[w.id as JudgeWindow]
                  return (
                    <td key={w.id} role="cell" data-label={w.label} className="num">
                      {c.total} 筆
                      <div className="hint">超過門檻 {c.over === null ? '—' : c.over}・錯誤 {c.errors}</div>
                    </td>
                  )
                })}
                <td role="cell" data-label="最後一次" className="num">
                  {st.last ? when(st.last) : '—'}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <h5>最近 20 筆</h5>
      <div className="judge-ledger-scroll">
        <table className="judge-ledger-table" role="table">
          <thead role="rowgroup">
            <tr role="row">
              <th role="columnheader">時間</th>
              <th role="columnheader">場景</th>
              <th role="columnheader">bot</th>
              <th role="columnheader">Jev 機率</th>
              <th role="columnheader">耗時 ms</th>
              <th role="columnheader">錯誤</th>
            </tr>
          </thead>
          <tbody role="rowgroup">
            {recent.map((r) => (
              <tr key={r.id} role="row" className={overThreshold(r) === true ? 'over' : undefined}>
                <td role="cell" data-label="時間" className="num">
                  {when(r.at)}
                </td>
                <td role="cell" data-label="場景">{sceneName(r.regex_verdict)}</td>
                <td role="cell" data-label="bot">{botLabel(r.bot_id, bots)}</td>
                <td role="cell" data-label="Jev 機率" className="num">
                  {probabilityText(r)}
                </td>
                <td role="cell" data-label="耗時 ms" className="num">
                  {r.ms ?? '—'}
                </td>
                <td role="cell" data-label="錯誤">{r.error ? r.error : '—'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  )
}
