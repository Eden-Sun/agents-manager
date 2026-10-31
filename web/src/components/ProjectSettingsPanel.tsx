import { useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Bot } from '../api/types'
import { useBotLamp } from '../hooks/useBotLamp'
import { useT } from '../lib/i18n'
import { projectHostName, useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { HostBadge } from './HostsPanel'
import { KindIcon } from './KindTag'
import { ProjectExpertSettings } from './ProjectExpertSettings'
import { ProjectNameField } from './ProjectNameField'
import { projectDeleteBlockers } from './projectDeleteGuard'
import { StatusLamp } from './StatusLamp'
import './projectSettingsPanel.css'

function Member({ bot }: { bot: Bot }) {
  const t = useT()
  const selectBot = useStore((s) => s.selectBot)
  const { lamp, background, label, kids } = useBotLamp(bot.id)
  const status = t('{name}：{status}', { name: bot.name, status: t.tr(label) })
  return <button type="button" className="btn project-settings-member" title={status} onClick={() => selectBot(bot.id)}>
    <KindIcon kind={bot.kind} /> <StatusLamp lamp={lamp} background={background} kids={kids} title={status} /> {bot.name}
  </button>
}

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
      {onOpenSidebar ? <button type="button" className="btn menu-btn icon-tip" onClick={onOpenSidebar} aria-label={t('開啟側邊欄')}>☰</button> : null}
      <div className="main-title"><ProjectNameField projectId={projectId} label={project.label} editing={editing} onEditing={setEditing} /> <HostBadge host={host} connected={connected} /></div>
    </div>
    <div className="project-settings">
      <h2>{t('專案設定')}</h2>
      <p>{t('路徑')}：<code>{project.path}</code></p>
      <ProjectExpertSettings projectId={projectId} />
      <h3>{t('成員')}</h3>
      <div className="project-settings-members" role="list" aria-label={t('專案成員')}>
        {members.length ? members.map((bot) => <span role="listitem" key={bot.id}><Member bot={bot} /></span>) : <span className="hint">{t('（尚無 Bot）')}</span>}
      </div>
      <button type="button" className="btn" onClick={() => setDeleting(true)}>{t('刪除專案…')}</button>
    </div>
    <ConfirmDialog open={deleting} title={t('刪除 Project')} confirmLabel={t('刪除 Project')} danger confirmDisabled={blockers.active > 0}
      body={<>{t.x('要把 {name}{target} 從清單移除嗎？磁碟上的目錄與其中的檔案都不會動{children}。', { name: <strong>{project.label}</strong>, target: t.x('（{host} {path}）', { host: host === 'local' ? t('本機') : host, path: <code>{project.path}</code> }), children: blockers.total > 0 ? t('，底下 {n} 個 Bot 的設定會一起移除', { n: blockers.total }) : '' })}{blockers.active > 0 ? <><br /><strong>{t('仍有 {n} 個 Bot 在跑', { n: blockers.active })}</strong>{t('，需先停止才能刪除。')}</> : null}</>}
      onCancel={() => setDeleting(false)} onConfirm={() => { setDeleting(false); void removeProject(projectId) }} />
  </>
}
