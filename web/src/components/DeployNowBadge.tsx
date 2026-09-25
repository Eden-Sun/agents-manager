/**
 * 左上角「立即部署」（使用者 2026-09-25：「agm排以外，要我可以在左上角直接點立即部署」）。
 *
 * 線上 binary 落後 origin/main、而且有會進 binary 的差異時才出現，寫落後幾個 commit；按下去先開確認框
 * （要上線的 commit 範圍、正在 working 的 bot），確認後 `POST /api/deploy/now`——daemon 以使用者的名義核准
 * 並叫起既有的例行更新（daemon-update-kick），安全條件一條都不省。部署在跑時 chip 改寫「部署中」、不給再按。
 */
import { useCallback, useEffect, useState } from 'react'
import { fetchDeployStatus, startDeployNow } from '../api/deploy'
import { useStore } from '../store/store'
import { deployNowNotice, deployVisible, runningText, type DeployStatus } from '../lib/deployNow'
import { ConfirmDialog } from './ConfirmDialog'
import './deployNow.css'

/** origin/main 約 5 分鐘一個 push；一分鐘看一次夠了，開確認框時另外當場重讀。 */
const POLL_MS = 60_000

export function DeployNowBadge() {
  const [status, setStatus] = useState<DeployStatus | null>(null)
  const [offline, setOffline] = useState(false)
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const notify = useStore((s) => s.notify)

  const refresh = useCallback(async () => {
    const snap = await fetchDeployStatus()
    setOffline(snap.offline)
    // 連不上就留著上一次的（標成過期、不給按）；daemon 自己說沒有才收掉。
    if (!snap.offline) setStatus(snap.status)
  }, [])

  useEffect(() => {
    // `refresh` 不會拋（fetchDeployStatus 已經吞掉），這裡再接一次免得變成 unhandled rejection。
    const tick = () => void refresh().catch(() => {})
    tick()
    const t = setInterval(tick, POLL_MS)
    return () => clearInterval(t)
  }, [refresh])

  if (!deployVisible(status) || !status) return null
  const running = status.running
  // 側欄標題列只放得下 `⇪N`；完整說法在 aria-label 與 title。
  const label = running ? '部署中' : `${status.behind}`
  const title = offline
    ? '連不上 daemon（可能正在重啟），這是斷線前的狀態。'
    : running
      ? `${runningText(running)}。`
      : `線上 ${status.live_sha} 落後 origin/main ${status.behind} 個 commit（${status.code_commits} 個動到程式碼）：點一下立即部署。`

  const confirm = () => {
    setOpen(false)
    setBusy(true)
    void (async () => {
      let n
      try {
        n = deployNowNotice(await startDeployNow(status.target_sha), null)
      } catch (e) {
        n = deployNowNotice(null, e)
      }
      notify(n.level, n.text)
      setBusy(false)
      await refresh().catch(() => {})
    })()
  }

  return (
    <>
      <button
        type="button"
        className={`deploy-badge${running ? ' running' : ''}${offline ? ' offline' : ''}`}
        aria-label={running ? `部署中：${runningText(running)}` : `立即部署：落後 ${status.behind} 個 commit`}
        title={title}
        disabled={busy}
        onClick={() => {
          if (running) {
            notify('info', `${runningText(running)}。進度看 ${status.log_path}`)
            return
          }
          if (offline) return
          // 框裡的範圍要是現況：當場重讀一次再打開。
          void refresh().catch(() => {}).finally(() => setOpen(true))
        }}
      >
        <span className="deploy-k" aria-hidden="true">⇪</span>
        <span className="deploy-v">{busy ? '送出中' : label}</span>
      </button>
      <ConfirmDialog
        open={open && !running}
        title="立即部署"
        width={420}
        confirmLabel="立即部署"
        confirmDisabled={!status.kick_ready || !status.code_changed}
        onCancel={() => setOpen(false)}
        onConfirm={confirm}
        body={
          <div className="deploy-confirm">
            <p>
              線上 <code>{status.live_sha}</code> → 上線 <code>{status.target_short}</code>（origin/main），共{' '}
              <strong>{status.behind}</strong> 個 commit，其中 <strong>{status.code_commits}</strong> 個動到程式碼。
            </p>
            <ul className="deploy-commits">
              {status.commits.map((c) => (
                <li key={c.sha}>
                  <code>{c.sha}</code> <span title={c.subject}>{c.subject}</span>
                </li>
              ))}
              {status.commits_truncated ? <li className="deploy-more">…還有 {status.behind - status.commits.length} 個</li> : null}
            </ul>
            <p className={status.working.length ? 'deploy-working' : undefined}>
              {status.working.length
                ? `正在 working：${status.working.map((w) => w.name).join('、')}——換 binary 會等它們跑完（等太久照例行規則縮小封鎖面），不會中途砍掉。`
                : '目前沒有 bot 在 working。'}
            </p>
            <p className="deploy-note">
              不等排程、不等 AGM 裁示；照例行流程在乾淨 worktree 建置、整樹測試、備份 .bak，驗證失敗自動回滾。進度看{' '}
              <code>{status.log_path}</code>
            </p>
            {!status.kick_ready ? (
              <p className="deploy-working">裝好的 daemon-update-kick.sh 還不認得立即部署，要先照 scripts/ops/README.md install 新版。</p>
            ) : null}
          </div>
        }
      />
    </>
  )
}
