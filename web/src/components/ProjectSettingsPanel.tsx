import { useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import { useT } from '../lib/i18n'
import { projectHostName, useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { HostBadge } from './HostsPanel'
import { ProjectExpertSettings } from './ProjectExpertSettings'
import { ProjectNameField } from './ProjectNameField'
import { projectDeleteBlockers } from './projectDeleteGuard'
import { QuotaStrip } from './QuotaStrip'
import { UnreadChip } from './UnreadChip'
import './projectSettingsPanel.css'

export function ProjectSettingsPanel({ projectId, onOpenSidebar }: { projectId: string; onOpenSidebar?: () => void }) {
  const t = useT()
  const project = useStore((s) => s.projects.find((p) => p.id === projectId))
  const host = useStore((s) => projectHostName(s, projectId))
  const connected = useStore((s) => host === 'local' || (s.hosts.find((h) => h.name === host)?.connected ?? false))
  const members = useStore(useShallow((s) => s.bots.filter((b) => b.project_id === projectId)))
  const runs = useStore((s) => s.runs)
  const removeProject = useStore((s) => s.removeProject)
  const [editing, setEditing] = useState(false)
  const [deleting, setDeleting] = useState(false)
  if (!project) return <div className="main-head"><span>{t('找不到 Project')}</span></div>
  const blockers = projectDeleteBlockers(members, runs, projectId)
  return <>
    <div className="main-head">
      {onOpenSidebar ? <button type="button" className="btn menu-btn icon-tip" onClick={onOpenSidebar} aria-label={t('開啟側邊欄')} data-tip={t('開啟側邊欄')}>☰</button> : null}
      <div className="main-title"><ProjectNameField projectId={projectId} label={project.label} editing={editing} onEditing={setEditing} /> <HostBadge host={host} connected={connected} /></div>
      <span className="spacer" />
      <QuotaStrip />
    </div>
    <UnreadChip />
    <div className="project-settings">
      <h2>{t('專案設定')}</h2>
      <p>{t('路徑')}：<code>{project.path}</code></p>
      <ProjectExpertSettings projectId={projectId} />
      <button type="button" className="btn" onClick={() => setDeleting(true)}>{t('刪除專案…')}</button>
    </div>
    <ConfirmDialog open={deleting} title={t('刪除 Project')} confirmLabel={t('刪除 Project')} danger confirmDisabled={blockers.active > 0}
      body={<>{t.x('要把 {name}{target} 從清單移除嗎？磁碟上的目錄與其中的檔案都不會動{children}。', { name: <strong>{project.label}</strong>, target: t.x('（{host} {path}）', { host: host === 'local' ? t('本機') : host, path: <code>{project.path}</code> }), children: blockers.total > 0 ? t('，底下 {n} 個 Bot 的設定會一起移除', { n: blockers.total }) : '' })}{blockers.active > 0 ? <><br /><strong>{t('仍有 {n} 個 Bot 在跑', { n: blockers.active })}</strong>{t('，需先停止才能刪除。')}</> : null}</>}
      onCancel={() => setDeleting(false)} onConfirm={() => { setDeleting(false); void removeProject(projectId) }} />
  </>
}
