import { useCallback, useEffect, useState } from 'react'
import * as api from '../api'
import type { ProjectPane } from '../api'
import { useStore } from '../store/store'
import './projectPanes.css'

/** 「上次有輸出」到現在多久：只給人看，粗略就好。 */
function idleFor(at: string): string {
  const ms = Date.now() - new Date(at).getTime()
  if (!Number.isFinite(ms) || ms < 0) return '—'
  const m = Math.floor(ms / 60000)
  if (m < 1) return '剛剛'
  if (m < 60) return `${m} 分鐘`
  const h = Math.floor(m / 60)
  return h < 48 ? `${h} 小時` : `${Math.floor(h / 24)} 天`
}

const OWNER_LABEL: Record<ProjectPane['owned_by'], string> = {
  bot: 'Bot 開的',
  user: '你手開的',
  none: '沒有歸屬',
}

/**
 * 專案頁的「其他 pane」（SPEC §6.5e）：這個專案底下不是 agent 的 pane——bot 開的 shell／服務，
 * 以及你自己在專案目錄裡開的。列出用途、前景程式、port、擁有者、閒置多久、在哪個 workspace；
 * 可以聚焦或關閉。**服務 pane 關閉要再確認一次**，因為關掉它等於殺掉裡面在跑的東西。
 */
export function ProjectPanes({ projectId, workspaceId }: { projectId: string; workspaceId: string | null }) {
  const [panes, setPanes] = useState<ProjectPane[] | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [confirming, setConfirming] = useState<ProjectPane | null>(null)
  const bots = useStore((s) => s.bots)
  const notify = useStore((s) => s.notify)

  const load = useCallback(async () => {
    try {
      setPanes(await api.fetchProjectPanes(projectId))
    } catch {
      // 舊 daemon 沒有這支 API：整個區塊不出現，不要在專案頁丟錯誤。
      setPanes([])
    }
  }, [projectId])

  useEffect(() => {
    void load()
    const t = setInterval(() => void load(), 30_000)
    return () => clearInterval(t)
  }, [load])

  if (!panes || panes.length === 0) return null

  const act = async (p: ProjectPane, what: 'focus' | 'close', confirm = false) => {
    setBusy(p.pane_id)
    try {
      if (what === 'focus') await api.focusPane(p.pane_id, p.host)
      else await api.closePane(p.pane_id, p.host, confirm)
      await load()
    } catch (e) {
      notify('error', `${what === 'focus' ? '聚焦' : '關閉'}失敗：${e instanceof Error ? e.message : String(e)}`)
    } finally {
      setBusy(null)
      setConfirming(null)
    }
  }

  return (
    <section className="project-panes" aria-label="其他 pane">
      <h2 className="project-panes-title">
        其他 pane<span className="project-panes-note">不是 agent 的 shell 與服務</span>
      </h2>
      <ul className="project-panes-list">
        {panes.map((p) => {
          const owner = p.owner_bot_id ? bots.find((b) => b.id === p.owner_bot_id)?.name : null
          const elsewhere = workspaceId && p.workspace_id && p.workspace_id !== workspaceId
          return (
            <li key={`${p.host}:${p.pane_id}`} className={`project-pane ${p.kind}`}>
              <div className="project-pane-head">
                <span className={`pane-kind ${p.kind}`}>{p.kind === 'service' ? '服務' : 'shell'}</span>
                <span className="pane-name">{p.purpose || p.pane_id}</span>
                <span className="pane-owner">{owner ? `${OWNER_LABEL[p.owned_by]}：${owner}` : OWNER_LABEL[p.owned_by]}</span>
                {elsewhere ? (
                  <span className="pane-elsewhere" title="這顆 pane 不在本專案的 workspace，搬動會殺掉裡面在跑的東西">
                    在 {p.workspace_id}
                  </span>
                ) : null}
              </div>
              <div className="project-pane-meta">
                <span className="pane-id">{p.pane_id}</span>
                {p.foreground ? <span className="pane-fg" title={p.foreground}>{p.foreground}</span> : null}
                {p.listen_ports.length > 0 ? (
                  <span className="pane-ports">port {p.listen_ports.join('、')}</span>
                ) : null}
                <span className="pane-idle">閒置 {idleFor(p.last_output_at)}</span>
              </div>
              <div className="project-pane-actions">
                <button type="button" className="mini-btn" disabled={busy === p.pane_id} onClick={() => void act(p, 'focus')}>
                  聚焦
                </button>
                <button
                  type="button"
                  className="mini-btn danger"
                  disabled={busy === p.pane_id}
                  onClick={() => (p.kind === 'service' ? setConfirming(p) : void act(p, 'close'))}
                >
                  關閉
                </button>
              </div>
            </li>
          )
        })}
      </ul>
      {confirming ? (
        <div className="modal-backdrop" role="presentation" onMouseDown={() => setConfirming(null)}>
          <div className="modal pane-close-modal" role="dialog" aria-modal="true" onMouseDown={(e) => e.stopPropagation()}>
            <strong>關掉這顆服務 pane？</strong>
            <p>
              {confirming.purpose || confirming.pane_id} 正在跑{' '}
              <code>{confirming.foreground ?? '（讀不到前景程式）'}</code>
              {confirming.listen_ports.length > 0 ? (
                <>
                  ，listen <strong>port {confirming.listen_ports.join('、')}</strong>
                </>
              ) : null}
              。關掉會一起殺掉裡面在跑的東西。
            </p>
            <div className="modal-actions">
              <button type="button" className="btn" onClick={() => setConfirming(null)}>
                取消
              </button>
              <button type="button" className="btn danger" onClick={() => void act(confirming, 'close', true)}>
                關閉
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </section>
  )
}
