import { useEffect, useState } from 'react'
import { fetchChangelog, type ChangelogReply } from '../api/changelog'

/**
 * 「有更新 · 重啟套用」確認框裡的 changelog 區塊（2026-09-10 使用者需求：先看新版改了什麼，
 * 確認後才重啟）。三態：抓取中／找到了（一段一版，新的在前）／找不到（明講原因＋原始連結）。
 * 抓不到 changelog 不擋重啟——那是使用者的決定，這裡只負責把話講清楚。
 */
export function UpdateChangelog({ kind, host, from }: { kind: string; host: string; from: string | null }) {
  // 以「這次請求的 key」配對結果：key 換了就等於重新載入，不必在 effect 裡同步清狀態。
  const key = `${kind}|${host}|${from ?? ''}`
  const [state, setState] = useState<{ key: string; reply: ChangelogReply } | null>(null)

  useEffect(() => {
    let alive = true
    void fetchChangelog(kind, host, from).then((r) => {
      if (alive) setState({ key, reply: r })
    })
    return () => {
      alive = false
    }
  }, [kind, host, from, key])

  const reply = state?.key === key ? state.reply : null
  if (!reply) {
    return (
      <div className="update-changelog loading" role="status">
        正在取得新版 changelog…
      </div>
    )
  }

  const head =
    reply.installedVersion && reply.fromVersion && reply.installedVersion !== reply.fromVersion
      ? `${reply.fromVersion} → ${reply.installedVersion}`
      : reply.installedVersion
        ? `新版 ${reply.installedVersion}`
        : null

  if (!reply.found) {
    return (
      <div className="update-changelog missing" role="status">
        <strong>找不到 changelog</strong>
        {head ? <span className="update-changelog-ver">{head}</span> : null}
        <div className="update-changelog-why">{reply.error ?? '沒有拿到任何段落'}</div>
        <a href={reply.sourceUrl} target="_blank" rel="noreferrer">
          自己到 GitHub 看 CHANGELOG ↗
        </a>
      </div>
    )
  }

  return (
    <div className="update-changelog">
      <div className="update-changelog-head">
        <strong>這次更新改了什麼</strong>
        {head ? <span className="update-changelog-ver">{head}</span> : null}
      </div>
      <div className="update-changelog-body">
        {reply.sections.map((s) => (
          <section key={s.version}>
            <h4>{s.version}</h4>
            <pre>{s.body}</pre>
          </section>
        ))}
      </div>
      <a className="update-changelog-src" href={reply.sourceUrl} target="_blank" rel="noreferrer">
        完整 CHANGELOG ↗
      </a>
    </div>
  )
}
