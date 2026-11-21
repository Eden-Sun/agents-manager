import { useCallback, useEffect, useState } from 'react'
import { usePollWhileVisible } from '../hooks/usePollWhileVisible'
import { fetchDeployStatus, startDeployNow } from '../api/deploy'
import { useStore } from '../store/store'
import { deployNowNotice, deployVisible, runningText, type DeployStatus } from '../lib/deployNow'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'
import './deployNow.css'

const POLL_MS = 60_000

export function DeployNowBadge() {
  const t = useT()
  const [status, setStatus] = useState<DeployStatus | null>(null)
  const [offline, setOffline] = useState(false)
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const notify = useStore((s) => s.notify)

  const refresh = useCallback(async () => {
    const snap = await fetchDeployStatus()
    setOffline(snap.offline)
    if (!snap.offline) setStatus(snap.status)
  }, [])

  const tick = useCallback(() => void refresh().catch(() => {}), [refresh])
  useEffect(() => {
    tick()
  }, [tick])
  usePollWhileVisible(tick, POLL_MS)

  if (!deployVisible(status) || !status) return null
  const running = status.running
  const label = running ? t('部署中') : `${status.behind}`
  const title = offline
    ? t('連不上 daemon（可能正在重啟），這是斷線前的狀態。')
    : running
      ? t('{text}。', { text: runningText(running) })
      : t('線上 {live} 落後 origin/main {behind} 個 commit（{code} 個動到程式碼）：點一下立即部署。', {
          live: status.live_sha,
          behind: status.behind,
          code: status.code_commits,
        })

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
        aria-label={running ? t('部署中：{text}', { text: runningText(running) }) : t('立即部署：落後 {n} 個 commit', { n: status.behind })}
        title={title}
        disabled={busy}
        onClick={() => {
          if (running) {
            notify('info', t('{text}。換好後這裡會自動消失。', { text: runningText(running) }))
            return
          }
          if (offline) return
          void refresh().catch(() => {}).finally(() => setOpen(true))
        }}
      >
        <span className="deploy-k" aria-hidden="true">⇪</span>
        <span className="deploy-v">{busy ? t('送出中') : label}</span>
      </button>
      <ConfirmDialog
        open={open && !running}
        title={t('立即部署')}
        width={420}
        confirmLabel={t('立即部署')}
        confirmDisabled={!status.kick_ready || !status.code_changed}
        onCancel={() => setOpen(false)}
        onConfirm={confirm}
        body={
          <div className="deploy-confirm">
            <p>
              {t.x('線上 {live} → 上線 {target}（origin/main），共 {behind} 個 commit，其中 {code} 個動到程式碼。', {
                live: <code>{status.live_sha}</code>,
                target: <code>{status.target_short}</code>,
                behind: <strong>{status.behind}</strong>,
                code: <strong>{status.code_commits}</strong>,
              })}
            </p>
            <ul className="deploy-commits">
              {status.commits.map((c) => (
                <li key={c.sha}>
                  <code>{c.sha}</code> <span title={c.subject}>{c.subject}</span>
                </li>
              ))}
              {status.commits_truncated ? <li className="deploy-more">{t('…還有 {n} 個', { n: status.behind - status.commits.length })}</li> : null}
            </ul>
            <p className={status.working.length ? 'deploy-working' : undefined}>
              {status.working.length
                ? t('正在 working：{names}——不等它們跑完就換版；pane 不會關，正在送訊息時才等。', { names: status.working.map((w) => w.name).join(t('、')) })
                : t('目前沒有 bot 在 working。')}
            </p>
            <p className="deploy-note">
              {t('不等排程、不等 ubuntu-ci；在專用乾淨 checkout 建置（建置已在推 main 前測過），備份 binary 與 DB，驗證失敗自動回滾。')}
            </p>
            {!status.kick_ready ? (
              <p className="deploy-working">{t('裝好的 daemon-update-kick.sh 還不認得立即部署，要先照 scripts/ops/README.md install 新版。')}</p>
            ) : null}
          </div>
        }
      />
    </>
  )
}
