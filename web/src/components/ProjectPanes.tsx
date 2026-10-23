import { useEffect, useState } from 'react'
import { usePollWhileVisible } from '../hooks/usePollWhileVisible'
import * as api from '../api'
import type { ProjectPane } from '../api'
import { useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { msg, useT, type TFn } from '../lib/i18n'
import './projectPanes.css'

/** 「上次有輸出」到現在多久：只給人看，粗略就好。 */
function idleFor(at: string, t: TFn): string {
  const ms = Date.now() - new Date(at).getTime()
  if (!Number.isFinite(ms) || ms < 0) return '—'
  const m = Math.floor(ms / 60000)
  if (m < 1) return t('剛剛')
  if (m < 60) return t('{n} 分鐘', { n: m })
  const h = Math.floor(m / 60)
  return h < 48 ? t('{n} 小時', { n: h }) : t('{n} 天', { n: Math.floor(h / 24) })
}

const OWNER_LABEL: Record<ProjectPane['owned_by'], string> = {
  bot: msg('Bot 開的'),
  user: msg('你手開的'),
  none: msg('沒有歸屬'),
}

/**
 * 專案頁的「其他 pane」（SPEC §6.5e）：這個專案底下不是 agent 的 pane——bot 開的 shell／服務，
 * 以及你自己在專案目錄裡開的。列出用途、前景程式、port、擁有者、閒置多久、在哪個 workspace；
 * 可以聚焦或關閉。**服務 pane 關閉要再確認一次**，因為關掉它等於殺掉裡面在跑的東西。
 *
 * 清單跟側欄是 store 裡同一份（`sidePanes`）：這裡關掉的，側欄同時不見。對不到專案的不在這裡，在側欄底部。
 */
export function ProjectPanes({ projectId, workspaceId }: { projectId: string; workspaceId: string | null }) {
  const t = useT()
  const panes = useStore((s) => s.sidePanes[projectId])
  const refresh = useStore((s) => s.refreshPanes)
  const closeTracedPane = useStore((s) => s.closeTracedPane)
  const [busy, setBusy] = useState<string | null>(null)
  const [confirming, setConfirming] = useState<ProjectPane | null>(null)
  const bots = useStore((s) => s.bots)
  const notify = useStore((s) => s.notify)

  useEffect(() => {
    void refresh()
  }, [refresh, projectId])
  usePollWhileVisible(() => void refresh(), 30_000)

  if (!panes || panes.length === 0) return null

  const focus = async (p: ProjectPane) => {
    setBusy(p.pane_id)
    try {
      await api.focusPane(p.pane_id, p.host)
    } catch (e) {
      notify('error', t('聚焦失敗：{reason}', { reason: e instanceof Error ? e.message : String(e) }))
    } finally {
      setBusy(null)
    }
  }

  const close = async (p: ProjectPane, confirm = false) => {
    setBusy(p.pane_id)
    setConfirming(null)
    const r = await closeTracedPane(p, confirm)
    setBusy(null)
    // 清單上還是 shell、其實已經開起 dev server：daemon 擋下來，換成它附上的最新那列再問一次。
    if (r && r !== 'closed') setConfirming(r)
  }

  return (
    <section className="project-panes" aria-label={t('其他 pane')}>
      <h2 className="project-panes-title">
        {t('其他 pane')}<span className="project-panes-note">{t('不是 agent 的 shell 與服務')}</span>
      </h2>
      <ul className="project-panes-list">
        {panes.map((p) => {
          const owner = p.owner_bot_id ? bots.find((b) => b.id === p.owner_bot_id)?.name : null
          const elsewhere = workspaceId && p.workspace_id && p.workspace_id !== workspaceId
          const ownerText = owner ? t('{label}：{owner}', { label: t.tr(OWNER_LABEL[p.owned_by]), owner }) : t.tr(OWNER_LABEL[p.owned_by])
          return (
            <li key={`${p.host}:${p.pane_id}`} className={`project-pane ${p.kind}`}>
              <div className="project-pane-head">
                <span className={`pane-kind ${p.kind}`}>{p.kind === 'service' ? t('服務') : 'shell'}</span>
                <span className="pane-name">{p.purpose || p.pane_id}</span>
                <span className="pane-owner">{ownerText}</span>
                {elsewhere ? (
                  <span className="pane-elsewhere" title={t('這顆 pane 不在本專案的 workspace，搬動會殺掉裡面在跑的東西')}>
                    {t('在 {workspace}', { workspace: p.workspace_id ?? '' })}
                  </span>
                ) : null}
              </div>
              <div className="project-pane-meta">
                <span className="pane-id">{p.pane_id}</span>
                {p.foreground ? <span className="pane-fg" title={p.foreground}>{p.foreground}</span> : null}
                {p.listen_ports.length > 0 ? (
                  <span className="pane-ports">port {p.listen_ports.join(' · ')}</span>
                ) : null}
                <span className="pane-idle">{t('閒置 {time}', { time: idleFor(p.last_output_at, t) })}</span>
              </div>
              <div className="project-pane-actions">
                <button type="button" className="mini-btn" disabled={busy === p.pane_id} onClick={() => void focus(p)}>
                  {t('聚焦')}
                </button>
                <button
                  type="button"
                  className="mini-btn danger"
                  disabled={busy === p.pane_id}
                  onClick={() => (p.kind === 'service' ? setConfirming(p) : void close(p))}
                >
                  {t('關閉')}
                </button>
              </div>
            </li>
          )
        })}
      </ul>
      <ConfirmDialog
        open={confirming !== null}
        title={t('關掉這顆服務 pane？')}
        danger
        confirmLabel={t('關閉')}
        body={
          confirming ? (
            <>
              {t.x('{target} 正在跑 {code}{ports}。關掉會一起殺掉裡面在跑的東西。', {
                target: confirming.purpose || confirming.pane_id,
                code: <code>{confirming.foreground ?? t('（讀不到前景程式）')}</code>,
                ports: confirming.listen_ports.length > 0 ? (
                  <>
                    {t.x('，listen {port}', {
                      port: <strong>port {confirming.listen_ports.join(' · ')}</strong>,
                    })}
                  </>
                ) : '',
              })}
            </>
          ) : null
        }
        onConfirm={() => confirming && void close(confirming, true)}
        onCancel={() => setConfirming(null)}
      />
    </section>
  )
}
