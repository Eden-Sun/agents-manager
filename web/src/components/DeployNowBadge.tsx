/**
 * 左上角「立即部署」（使用者 2026-09-25：「agm排以外，要我可以在左上角直接點立即部署」）。
 *
 * 線上 binary 落後 origin/main、而且有會進 binary 的差異時才出現，寫落後幾個 commit；按下去先開確認框
 * （要上線的 commit 範圍、正在 working 的 bot），確認後 `POST /api/deploy/now`——daemon 寫下請求檔
 * 並叫起既有的自動部署（daemon-update-kick），直接部署那顆 commit、不等 ubuntu-ci，沒人 working 才換版等安全條件照舊。部署在跑時 chip 改寫「部署中」、不給再按。
 */
import { useCallback, useEffect, useState } from 'react'
import { usePollWhileVisible } from '../hooks/usePollWhileVisible'
import { fetchDeployStatus, startDeployNow } from '../api/deploy'
import { useStore } from '../store/store'
import { deployNowNotice, deployVisible, runningText, type DeployStatus } from '../lib/deployNow'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'
import './deployNow.css'

/** origin/main 約 5 分鐘一個 push；一分鐘看一次夠了，開確認框時另外當場重讀。 */
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
    // 連不上就留著上一次的（標成過期、不給按）；daemon 自己說沒有才收掉。
    if (!snap.offline) setStatus(snap.status)
  }, [])

  // `refresh` 不會拋（fetchDeployStatus 已經吞掉），這裡再接一次免得變成 unhandled rejection。
  const tick = useCallback(() => void refresh().catch(() => {}), [refresh])
  useEffect(() => {
    tick()
  }, [tick])
  usePollWhileVisible(tick, POLL_MS)

  if (!deployVisible(status) || !status) return null
  const running = status.running
  // 側欄標題列只放得下 `⇪N`；完整說法在 aria-label 與 title。
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
            notify('info', t('{text}。進度看 {log}', { text: runningText(running), log: status.log_path }))
            return
          }
          if (offline) return
          // 框裡的範圍要是現況：當場重讀一次再打開。
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
                ? t('正在 working：{names}——換 binary 會等它們跑完，不會中途砍掉。', { names: status.working.map((w) => w.name).join(t('、')) })
                : t('目前沒有 bot 在 working。')}
            </p>
            <p className="deploy-note">
              {t.x('不等排程、不等 ubuntu-ci；在專用乾淨 checkout 建置（建置已在推 main 前測過），備份 binary 與 DB，驗證失敗自動回滾。進度看 {log}', {
                log: <code>{status.log_path}</code>,
              })}
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
