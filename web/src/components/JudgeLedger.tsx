/**
 * 環境設定 → Jev 第二意見 底下的「帳本」（SPEC §4.3c，issue #1327）：四個場景各一列的計數，加最近 20 筆。
 * 只讀；不顯示遮罩後的命中行（那是給 daemon 對帳用的）。舊 daemon 沒有這個端點 → 說明，不報錯。
 */
import { useEffect, useState } from 'react'
import { fetchJudgeShadow, type JudgeShadow } from '../api/judge'
import { useStore } from '../store/store'
import { botLabel, JUDGE_SCENES, JUDGE_WINDOWS, overThreshold, probabilityText, recentRows, sceneStats, type JudgeWindow } from '../lib/judgeStats'

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
    fetchJudgeShadow()
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
      <div className="judge-ledger-scroll">
        <table className="judge-ledger-table">
          <thead>
            <tr>
              <th>場景</th>
              {JUDGE_WINDOWS.map((w) => (
                <th key={w.id}>{w.label}</th>
              ))}
              <th>最後一次</th>
            </tr>
          </thead>
          <tbody>
            {stats.map((st) => (
              <tr key={st.scene}>
                <td>
                  <b>{sceneName(st.scene)}</b>
                  <div className="hint">{JUDGE_SCENES.find((s) => s.id === st.scene)?.when}</div>
                </td>
                {JUDGE_WINDOWS.map((w) => {
                  const c = st.windows[w.id as JudgeWindow]
                  return (
                    <td key={w.id}>
                      {c.total} 筆
                      <div className="hint">超過門檻 {c.over === null ? '—' : c.over}・錯誤 {c.errors}</div>
                    </td>
                  )
                })}
                <td>{st.last ? when(st.last) : '—'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <h5>最近 20 筆</h5>
      <div className="judge-ledger-scroll">
        <table className="judge-ledger-table">
          <thead>
            <tr>
              <th>時間</th>
              <th>場景</th>
              <th>bot</th>
              <th>機率</th>
              <th>耗時 ms</th>
              <th>錯誤</th>
            </tr>
          </thead>
          <tbody>
            {recent.map((r) => (
              <tr key={r.id} className={overThreshold(r) === true ? 'over' : undefined}>
                <td>{when(r.at)}</td>
                <td>{sceneName(r.regex_verdict)}</td>
                <td>{botLabel(r.bot_id, bots)}</td>
                <td>{probabilityText(r)}</td>
                <td>{r.ms ?? '—'}</td>
                <td>{r.error ? r.error : '—'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  )
}
