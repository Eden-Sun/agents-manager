import { Fragment, memo, type ReactNode, useCallback, useEffect, useId, useMemo, useRef, useState } from 'react'
import { persistSetDiff } from '../store/sharedSet'
import { renderProbe } from '../lib/renderProbe'
import { useStableCallback } from '../hooks/useStableCallback'
import { BOT_NAME_HINT, isValidBotName } from '../lib/botName'
import { quotaIdentityLabel } from '../lib/defaultIdentity'
import { t, useT } from '../lib/i18n'
import { useShallow } from 'zustand/react/shallow'
import * as api from '../api'
import { MOCK_MODE } from '../api'
import type { Bot, BotKind, Lamp, MessageHit, ShareProfile } from '../api/types'
import { BOT_KINDS, CREATABLE_KINDS, LOCAL_HOST } from '../api/types'
import { identityBadgeVisible } from '../lib/identityBadgeVisible'
import { labelAfterPick } from '../lib/projectLabel'
import { eventIsFromCurrentTarget, eventTargetIsInsideCurrentTarget } from '../lib/domEvents'
import { herdrIdentity } from '../lib/herdrIdentity'
import { settingsBlocksLeave } from '../lib/settingsLeaveGuard'
import {
  adjacentBotId,
  botLamp,
  botQuotaLevel,
  botQuotaWarning,
  botMatches,
  botsOfProject,
  enabledIdentities,
  identitiesOfHost,
  orderedProjects,
  projectHostName,
  quotaClaimants,
  toolsOfHost,
  useStore,
} from '../store/store'
import type { SocketStatus } from '../store/store'
import { useBotLamp } from '../hooks/useBotLamp'
import { projectUnread, unreadShown } from '../store/unread'
import { ClockIcon, GearIcon, TerminalIcon } from './Icons'
import { StatusLamp } from './StatusLamp'
import { LAMP_LABEL } from './lampLabel'
import { ConfirmDialog } from './ConfirmDialog'
import { CAPACITY_LABEL, CAPACITY_TITLE, modelAtCapacity } from '../lib/modelCapacity'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { projectDeleteBlockers } from './projectDeleteGuard'
import { HeadMoreMenu } from './HeadMoreMenu'
import { DirPicker } from './DirPicker'
import { IdentityBadge } from './IdentitiesPanel'
import { Modal } from './Modal'
import { SidebarPanes, SidebarUnownedPanes } from './SidebarPanes'
import { ProjectAddMenu } from '../schedules/ProjectAddMenu'
import { listSchedules } from '../api/schedules'
import { parseScheduleUrl } from '../lib/pushNotify'
import { ProjectSchedules } from '../schedules/ProjectSchedules'
import { ScheduleEditor } from '../schedules/ScheduleEditor'
import { RunElapsed } from './RunElapsed'
import { IdentityOptions, PersonaField, PersonaMark } from './BotSettingsPanel'
import { ShareMark } from './ShareLinkButton'
import { FrozenBadge } from './FrozenBadge'
import { NO_WARM_TEXT } from '../lib/cacheClock'
import { EnvShell } from './EnvSettings'
import { HostBadge } from './HostsPanel'
import { ShareProfileField } from './ShareProfileField'
import { EMPTY_SHARE_FOLDER, shareCreateFields, shareFolderInput, shareProfileBlocked, type ShareFolderDraft } from '../lib/shareProfile'
import { HostDownState } from './HostOffline'
import { useBotOfflineHost } from '../hooks/useHostOffline'
import { BotNameField } from './BotNameField'
import { ProjectNameField } from './ProjectNameField'
import { HeadMemBadges } from './HeadMemBadges'
import { UpdateQuotaChip } from './UpdateQuotaChip'
import { ThemeToggle } from './ThemeToggle'
import { ProjectMemBadge } from './ProjectMemBadge'
import { HandedOffBadge } from './HandedOffBadge'
import { BackgroundJobsBadge } from './BackgroundJobs'
import { BotStateText } from './BotStateText'
import { DeployNowBadge } from './DeployNowBadge'
import { DeployWaitChip } from './DeployWaitChip'
import { TabsBadge } from './TabsBadge'
import { ModelTag } from './ModelTag'
import { BotRowMenu } from './BotRowMenu'
import { KindTag } from './KindTag'
import { KIND_LABEL, isApiKind } from './kindMeta'
import { QuickAddBots } from './QuickAddBots'
import { loginKindOnHost } from '../lib/onboardingActions'
import { ADD_PROJECT_EVENT } from '../lib/onboarding'
import { UpdateAllBanner } from './UpdateAllBanner'
import { ApiModelFields } from './ModelPicker'
import { InstallToolButton } from './Tools'
import { UpdateBadge } from './UpdateBadge'
import { ExpertExecutorSection } from './ExpertExecutorSection'
import { ExpertBadge, UnsetRoleBadge, ExpertDisabledMark, ExpertDeputyTag, ExpertRoleTag, ExpertSectionHeader } from './ExpertBadge'
import { canReorderInExpertMode, groupExpertSections } from '../lib/expertSections'
import { NewExpertBotForm } from './NewExpertBotForm'
import { quotaIdentity, runningFast, runtimeIdentity, runtimeSettingsKnown } from '../lib/runtimeDrift'
import { syncKidsScroll, wheelKidsScroll } from '../lib/kidsScroll'
import { createHitSearch } from '../lib/hitSearch'
import { useWheelRef } from '../hooks/useWheelRef'
import { scrollProjectIntoView } from '../lib/scrollProject'
import { botTopPending } from '../lib/scrollBot'
import { useBotScrollTop } from '../hooks/useBotScrollTop'
import './sidebar.css'

function ConnBadge({ socket, connected }: { socket: SocketStatus; connected: boolean }) {
  useT()
  const label =
    socket === 'open' ? (connected ? t('已連線') : t('herdr 中斷')) : socket === 'connecting' ? t('連線中') : t('重連中')
  return (
    <span className="conn" title={`WebSocket: ${socket} / herdr: ${connected ? 'connected' : 'disconnected'}`}>
      <span className={`conn-dot ${socket === 'open' && !connected ? 'closed' : socket}`} />
      <span className="conn-label">{label}</span>
    </span>
  )
}

function PaneBadge() {
  useT()
  const panes = useStore(
    useShallow((s) =>
      s.bots
        .filter((b) => {
          const r = s.runs[b.id]
          return Boolean(r) && r!.state !== 'stopped' && r!.state !== 'exited'
        })
        .map((b) => `${projectHostName(s, b.project_id)}\t${b.name}`),
    ),
  )
  if (panes.length === 0) return null

  const byHost = new Map<string, string[]>()
  for (const row of panes) {
    const [host, name] = row.split('\t')
    byHost.set(host, [...(byHost.get(host) ?? []), name])
  }
  const tip = [
    t('現在開著 {n} 個 herdr pane', { n: panes.length }),
    '',
    ...[...byHost.entries()].map(([host, names]) => t('{host}：{names}', { host: host === 'local' ? t('本機') : host, names: names.join(t('、')) })),
  ]
  return (
    <span className="pane-badge" title={tip.join('\n')}>
      <span className="pane-k">pane</span>
      <span className="pane-v">{panes.length}</span>
    </span>
  )
}

function shortPath(path: string, max = 30): string {
  const chars = Array.from(path)
  return chars.length <= max ? path : `…${chars.slice(-(max - 1)).join('')}`
}

type DragState = { id: string; projectId: string; overId: string | null; edge: 'before' | 'after' } | null

const BOT_ROW_HINT_ID = 'sidebar-bot-row-hint'

function BotRowImpl({
  botId,
  hit,
  childCount = 0,
  collapsed = false,
  kidsLamp = null,
  kidsWait = null,
  kidsListId,
  compact = false,
  onToggleChildren,
  drag,
  onDrag,
  onDropAt,
  onNudge,
  onStep,
}: {
  botId: string
  hit?: MessageHit
  childCount?: number
  collapsed?: boolean
  kidsLamp?: Lamp | null
  kidsWait?: KidsWait | null
  kidsListId?: string
  compact?: boolean
  onToggleChildren?: (botId: string) => void
  drag: DragState
  onDrag: (next: DragState) => void
  onDropAt: (dragId: string, overId: string, edge: 'before' | 'after') => void
  onNudge: (botId: string, dir: -1 | 1) => void
  onStep: (botId: string, dir: -1 | 1) => void
}) {
  const t = useT()
  renderProbe('BotRow')
  const bot = useStore((s) => s.bots.find((b) => b.id === botId))
  const project = useStore((s) => s.projects.find((p) => p.id === bot?.project_id))
  const isExpertProject = project?.mode === 'expert'
  const run = useStore((s) => s.runs[botId] ?? null)
  const frozen = bot?.primary === true && run?.keep_warm_skip === true
  const { lamp, background: bgJobs, label: stateLabel, blockedReason, kids: kidsRunning, compacting } = useBotLamp(botId)
  const hostDown = useBotOfflineHost(botId)
  const quotaWarning = useStore(
    useShallow((s) => {
      const b = s.bots.find((x) => x.id === botId)
      if (!b) return null
      const host = projectHostName(s, b.project_id)
      return botQuotaWarning(s.quota, b.kind, quotaIdentity(b, s.runs[botId] ?? null), host, quotaClaimants(s, host))
    }),
  )
  const quotaLevel = useStore(
    useShallow((s) => {
      const b = s.bots.find((x) => x.id === botId)
      const run = s.runs[botId] ?? null
      const model = run?.status?.model_name ?? (runtimeSettingsKnown(run) ? run!.runtime_model : (b?.model ?? null))
      if (!b) return null
      const host = projectHostName(s, b.project_id)
      return botQuotaLevel(s.quota, b.kind, quotaIdentity(b, run), host, model, quotaClaimants(s, host))
    }),
  )
  const selected = useStore((s) => s.selectedBotId === botId)
  const unread = useStore((s) => {
    return unreadShown(s.bots.find((x) => x.id === botId), s.supervisorProjectId) ? (s.botUnread[botId] ?? 0) : 0
  })
  const selectBot = useStore((s) => s.selectBot)
  const agentTitle = useStore((s) => {
    const r = s.runs[botId]
    const t = r?.agent_title?.trim()
    if (!t) return ''
    const l = botLamp(s, botId)
    return l === 'working' || l === 'idle' ? t : ''
  })

  const hasUpdate = useStore((s) => s.runs[botId]?.update_notice ?? null)
  const divergedChildren = useStore(
    useShallow((s) => {
      const b = s.bots.find((x) => x.id === botId)
      if (!b || b.kind !== 'claude') return null
      const mine = b.identity ?? 'cc0'
      const kids = s.bots.filter((x) => x.parent_bot_id === botId && x.kind === 'claude' && (x.identity ?? 'cc0') !== mine)
      if (kids.length === 0) return null
      const identities = [...new Set(kids.map((x) => x.identity ?? 'cc0'))].sort()
      return { identities: identities.join('/'), names: kids.map((x) => x.name).join(t('、')) }
    }),
  )
  const turnError = useStore((s) => s.runs[botId]?.turn_error ?? null)
  const fast = useStore((s) => {
    const b = s.bots.find((x) => x.id === botId)
    return b?.kind === 'codex' && runningFast(b, s.runs[botId] ?? null)
  })
  const capacity = useStore((s) => modelAtCapacity(s.runs[botId]))
  const runningIdentity = runtimeIdentity(run)
  const identityUnknown = Boolean(run && (run.state === 'running' || run.state === 'starting') && run.runtime_identity === null)
  const showIdentity = useStore(
    useShallow((s) => {
      const b = s.bots.find((x) => x.id === botId)
      if (!b) return false
      const host = projectHostName(s, b.project_id)
      const hostIds = identitiesOfHost(s.identities, host === LOCAL_HOST ? s.localIdentityStatus : {}, host)
      const used = s.bots.filter((x) => x.kind === b.kind && projectHostName(s, x.project_id) === host).map((x) => x.identity)
      return identityBadgeVisible(b.kind, hostIds, used)
    }),
  )

  if (!bot) return null
  if (bot.pending) {
    return (
      <div className="bot-row pending" role="listitem" aria-busy="true" data-bot-id={botId}>
        <StatusLamp lamp="starting" title={t('{name}：建立中', { name: bot.name })} />
        <span className="bot-main">
          <span className="bot-ident">
            <KindTag kind={bot.kind} className="bot-kind" />
            <span className="bot-name">{bot.name}</span>
          </span>
          <span className="bot-sub">
            <span className="bot-pending-note">{t('建立中…')}</span>
          </span>
        </span>
      </div>
    )
  }
  const showTitle = Boolean(agentTitle) && selected && !compact

  const dragging = drag?.id === botId
  const dragBot = drag ? useStore.getState().bots.find((b) => b.id === drag.id) : null
  const canReorderWithDrag = !isExpertProject || !dragBot || canReorderInExpertMode(dragBot, bot)
  const sameProject = drag?.projectId === bot.project_id && canReorderWithDrag
  const dropEdge = drag && sameProject && drag.id !== botId && drag.overId === botId ? drag.edge : null

  const edgeAt = (e: { currentTarget: HTMLElement; clientY: number }): 'before' | 'after' => {
    const r = e.currentTarget.getBoundingClientRect()
    return e.clientY < r.top + r.height / 2 ? 'before' : 'after'
  }

  const quotaWho = quotaIdentity(bot, run)
  const isExpertMemberWithManagerUse = isExpertProject && (bot.role === 'advisor' || bot.role === 'executor' || bot.role === 'specialist')

  return (
    <div
      className={`bot-row${compact ? ' compact' : ''}${isExpertMemberWithManagerUse && !compact ? ' expert-member' : ''}${selected ? ' selected' : ''}${dragging ? ' dragging' : ''}${
        dropEdge ? ` drop-${dropEdge}` : ''
      }${quotaWarning ? ' quota-critical' : ''}${childCount > 0 ? ' has-kids' : ''}${isExpertMemberWithManagerUse && bot.manager_use === false ? ' manager-disabled' : ''}`}
      role="listitem"
      aria-current={selected ? 'true' : undefined}
      aria-owns={kidsListId}
      aria-describedby={compact ? undefined : BOT_ROW_HINT_ID}
      aria-label={compact ? bot.name : undefined}
      data-bot-id={botId}
      tabIndex={0}
      draggable={!compact}
      onClick={(e) => {
        if (!eventTargetIsInsideCurrentTarget(e)) return
        selectBot(botId)
      }}
      onKeyDown={(e) => {
        if (!eventIsFromCurrentTarget(e)) return
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          if (settingsBlocksLeave()) return
          selectBot(botId)
        }
        if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return
        const dir = e.key === 'ArrowUp' ? -1 : 1
        if (e.altKey) {
          e.preventDefault()
          if (!compact) onNudge(botId, dir)
          return
        }
        e.preventDefault()
        if (settingsBlocksLeave()) return
        onStep(botId, dir)
      }}
      onDragStart={(e) => {
        e.dataTransfer.effectAllowed = 'move'
        e.dataTransfer.setData('text/plain', botId)
        onDrag({ id: botId, projectId: bot.project_id, overId: null, edge: 'before' })
      }}
      onDragEnd={() => onDrag(null)}
      onDragOver={(e) => {
        if (!drag || drag.id === botId || !sameProject) return
        e.preventDefault()
        e.dataTransfer.dropEffect = 'move'
        const edge = edgeAt(e)
        if (drag.overId !== botId || drag.edge !== edge) onDrag({ ...drag, overId: botId, edge })
      }}
      onDrop={(e) => {
        if (!drag || drag.id === botId || !sameProject) return
        e.preventDefault()
        onDropAt(drag.id, botId, edgeAt(e))
        onDrag(null)
      }}
    >
      {}
      <span className="bot-gutter">
        <span className="bot-gutter-top">
          <StatusLamp lamp={lamp} background={bgJobs} kids={kidsRunning} capacity={capacity} title={t('{name}：{status}{title}', { name: bot.name, status: capacity && lamp === 'idle' ? t.tr(CAPACITY_LABEL) : stateLabel, title: agentTitle && !showTitle ? t(' · {title}', { title: agentTitle }) : '' })} />
          {}
          {kidsWait ? (
            <span
              className={`bot-kids-wait ${kidsWait}`}
              title={
                kidsWait === 'busy'
                  ? t('{name} 在等底下的子 agent 做完', { name: bot.name })
                  : t('{name} 的子 agent 回報了，還沒有人看', { name: bot.name })
              }
              aria-label={kidsWait === 'busy' ? t('等子 agent 完成') : t('子 agent 已回報')}
            />
          ) : null}
          {}
          {unread > 0 ? (
            <span className="unread-turns" title={t('{n} 個回合已完成，還沒看過', { n: unread })}>
              !{unread > 99 ? '99+' : unread}
            </span>
          ) : null}
        </span>
        {}
        <RunElapsed botId={botId} />
        {childCount > 0 && onToggleChildren ? (
        <button
          type="button"
          className={`bot-kids-toggle${collapsed ? ' shut' : ''}`}
          aria-expanded={!collapsed}
          title={
            collapsed
              ? t('展開 {n} 個子 agent{lamp}', { n: childCount, lamp: kidsLamp ? t('（有子 agent {status}）', { status: t.tr(LAMP_LABEL[kidsLamp]) }) : '' })
              : t('收合 {n} 個子 agent', { n: childCount })
          }
          onClick={(e) => {
            e.stopPropagation()
            onToggleChildren(botId)
          }}
        >
          <span className="chev">{collapsed ? '▶' : '▼'}</span>
          {collapsed ? <span className="bot-kids-n">{childCount}</span> : null}
          {}
          {collapsed && kidsLamp ? <span className={`bot-kids-lamp lamp lamp-${kidsLamp}`} aria-hidden="true" /> : null}
        </button>
        ) : null}
      </span>
      <span className="bot-main" onScroll={compact ? syncKidsScroll : undefined}>
        <span className="bot-ident">
          {}
          <span className={`bot-kind-wrap${hasUpdate ? ' has-update' : ''}`}>
            <KindTag kind={bot.kind} className="bot-kind" fast={fast} />
            <UpdateBadge botId={botId} variant="dot" />
            {}
            {showIdentity ? (
              divergedChildren ? (
                <span
                  className="kind-identity identity-diverged"
                  title={t('底下有子 bot 用別的帳號（母 {parent}、子 {count}：{names}）——額度分開算，注意別把那個帳號用光', { parent: bot.identity ?? 'cc0', count: divergedChildren.identities, names: divergedChildren.names })}
                >
                  <IdentityBadge
                    name={runningIdentity === undefined ? bot.identity : runningIdentity}
                    showDefault
                    kind={bot.kind}
                    unknown={identityUnknown}
                  />
                  <span className="identity-diverged-mark" aria-hidden="true">{divergedChildren.identities}</span>
                </span>
              ) : (
                <span className="kind-identity">
                  <IdentityBadge
                    name={runningIdentity === undefined ? bot.identity : runningIdentity}
                    showDefault
                    kind={bot.kind}
                    unknown={identityUnknown}
                  />
                </span>
              )
            ) : null}
          </span>
          {}
          <BotNameField botId={botId} name={bot.name} variant="row" armed={false} hint={herdrIdentity(run, bot.agent_name)?.title}>
            {}
            {isExpertProject && !compact && bot.role_pending ? <ExpertRoleTag role={bot.role} rolePending /> : null}
            {}
            {isExpertProject && !compact && !bot.parent_bot_id && !bot.role && !bot.scheduled ? <UnsetRoleBadge botId={botId} /> : null}
            {}
            {isExpertProject && compact && bot.parent_bot_id && bot.role ? <ExpertRoleTag role={bot.role} /> : null}
            {compact ? null : <PersonaMark persona={bot.persona} />}
            <ShareMark botId={botId} />
            {frozen ? <FrozenBadge title={NO_WARM_TEXT} /> : null}
            {}
            {capacity ? (
              <span className="bot-capacity" title={t('{error}｜{title}', { error: turnError ?? '', title: t.tr(CAPACITY_TITLE) })}>
                {t.tr(CAPACITY_LABEL)}
              </span>
            ) : turnError ? (
              <span className="bot-turn-error" title={t('{error}｜這一回合被 API 中斷，回應不完整。點進去可以重送上一則', { error: turnError })}>
                {t('⚠ 中斷')}
              </span>
            ) : null}
            {}
            {showTitle ? null : compact ? (
              <BackgroundJobsBadge botId={botId} variant="mini" />
            ) : (
              hostDown ? <HostDownState /> : <BackgroundJobsBadge botId={botId} variant="state" fallback={<BotStateText lamp={lamp} reason={blockedReason} compacting={compacting} />} />
            )}
          </BotNameField>
          {isExpertProject && bot.manager_use === false ? <ExpertDisabledMark /> : null}
        </span>
        {hit ? (
          <span className="bot-hit" title={t('對話中有 {n} 則提到', { n: hit.hits })}>
            <span className="bot-hit-n">{hit.hits}</span>
            <span className="bot-hit-text">{hit.snippet}</span>
          </span>
        ) : null}
        <span className="bot-sub">
          {isExpertProject && project?.deputy?.bot_id === bot.id ? <ExpertDeputyTag afterSec={project.deputy.after_sec} /> : null}
          <ModelTag botId={botId} short={compact} cache />
          {quotaWarning ? (
            <span
              className="bot-quota-chip crit"
              title={t('{kind}{who} {window} 額度剩 {pct}%，快用完了', { kind: KIND_LABEL[bot.kind], who: quotaWho ? t(' · {who}', { who: quotaWho }) : '', window: quotaWarning.window, pct: quotaWarning.pct })}
            >
              ⚠ {quotaWarning.pct}%
            </span>
          ) : quotaLevel ? (
            <span
              className={`bot-quota-chip ${quotaLevel.level}`}
              title={t('{kind}{who} {window} 額度剩 {pct}%', { kind: KIND_LABEL[bot.kind], who: quotaWho ? t(' · {who}', { who: quotaWho }) : '', window: quotaLevel.window, pct: quotaLevel.pct })}
            >
              {quotaLevel.window} {quotaLevel.pct}%
            </span>
          ) : null}
        </span>
        {}
        {showTitle ? (
          <span className="bot-agent-title" title={t('agent 目前的標題：{title}', { title: agentTitle })}>
            {agentTitle}
          </span>
        ) : null}
      </span>
      <BotRowMenu botId={botId} compact={compact} />

    </div>
  )
}

const BotRow = memo(BotRowImpl)

function NewProjectForm({ onDone }: { onDone: () => void }) {
  useT()
  const addProject = useStore((s) => s.addProject)
  const hosts = useStore((s) => s.hosts)
  const projects = useStore((s) => s.projects)
  const [path, setPath] = useState('')
  const [label, setLabel] = useState('')
  const [host, setHost] = useState('local')
  const [busy, setBusy] = useState(false)
  const [browsing, setBrowsing] = useState(false)
  const autoLabel = useRef('')

  const hostOk = host === 'local' || hosts.some((h) => h.name === host)
  const effectiveHost = hostOk ? host : 'local'

  const linuxLike = useMemo(() => {
    if (projects.some((p) => p.path.startsWith('/home/'))) return true
    if (typeof navigator !== 'undefined' && /linux|x11/i.test(navigator.userAgent) && !/mac/i.test(navigator.userAgent)) return true
    return false
  }, [projects])

  if (browsing) {
    return (
      <DirPicker
        initial={path}
        host={effectiveHost}
        onCancel={() => setBrowsing(false)}
        onPick={(p) => {
          setPath(p)
          const next = labelAfterPick(label, autoLabel.current, p)
          if (next !== label) autoLabel.current = next
          setLabel(next)
          setBrowsing(false)
        }}
      />
    )
  }

  return (
    <form
      className="form"
      onSubmit={(e) => {
        e.preventDefault()
        if (!path.trim() || busy) return
        setBusy(true)
        void addProject({ path: path.trim(), label: label.trim(), host: effectiveHost }).then((ok) => {
          setBusy(false)
          if (ok) {
            setPath('')
            setLabel('')
            onDone()
          }
        })
      }}
    >
      <label className="field">
        <span>{t('所屬主機')}</span>
        <select
          value={effectiveHost}
          onChange={(e) => {
            setHost(e.target.value)
            setPath('')
            if (label === autoLabel.current) {
              setLabel('')
              autoLabel.current = ''
            }
          }}
        >
          <option value="local">{t('本機')}</option>
          {hosts.map((h) => (
            <option key={h.name} value={h.name} disabled={!h.connected}>
              {h.name}{t('（{ssh}）', { ssh: h.ssh })}{h.connected ? '' : t(' — 未連線')}
            </option>
          ))}
        </select>
      </label>
      <label className="field">
        <span>{t('目錄路徑')}{effectiveHost === 'local' ? '' : t('（{host} 上的絕對路徑）', { host: effectiveHost })}</span>
        <div className="field-with-btn">
          <input
            type="text"
            value={path}
            placeholder={linuxLike ? '/home/me/project/foo' : '/Users/me/project/foo'}
            spellCheck={false}
            onChange={(e) => setPath(e.target.value)}
          />
          <button type="button" className="btn" onClick={() => setBrowsing(true)} title={t('瀏覽目錄')}>
            {t('瀏覽…')}
          </button>
        </div>
      </label>
      <label className="field">
        <span>{t('標籤（留白則取目錄名）')}</span>
        <input type="text" value={label} placeholder="foo" onChange={(e) => setLabel(e.target.value)} />
      </label>
      <div className="form-actions">
        <button type="button" className="btn" onClick={onDone}>
          {t('取消')}
        </button>
        <button type="submit" className="btn primary" disabled={!path.trim() || busy}>
          {t('新增')}
        </button>
      </div>
    </form>
  )
}

function lastKindKey(projectId: string): string {
  return `am:lastKind:${projectId}`
}

function uniqueBotName(kind: BotKind, projectId: string, bots: { project_id: string; name: string }[]): string {
  const taken = new Set(bots.filter((b) => b.project_id === projectId).map((b) => b.name))
  let n = 1
  while (taken.has(`${kind}-${n}`)) n += 1
  return `${kind}-${n}`
}

function defaultKindForProject(projectId: string, tools: ReturnType<typeof toolsOfHost>, bots: { project_id: string; kind: BotKind }[]): BotKind {
  const installed = BOT_KINDS.filter((k) => tools[k].installed)
  try {
    const stored = localStorage.getItem(lastKindKey(projectId)) as BotKind | null
    if (stored && installed.includes(stored)) return stored
  } catch {
  }
  for (let i = bots.length - 1; i >= 0; i -= 1) {
    const b = bots[i]
    if (b.project_id === projectId && tools[b.kind].installed) return b.kind
  }
  return installed[0] ?? BOT_KINDS[0]
}

export function NewBotForm({ onDone, initialProjectId, onCreated, scheduler, defaultName }: { onDone: () => void; initialProjectId?: string; onCreated?: (botId: string) => void; scheduler?: boolean; defaultName?: string }) {
  const t = useT()
  const projects = useStore((s) => s.projects)
  const hosts = useStore((s) => s.hosts)
  const bots = useStore((s) => s.bots)
  const addBot = useStore((s) => s.addBot)
  const startBot = useStore((s) => s.startBot)
  const [projectId, setProjectId] = useState(initialProjectId ?? projects[0]?.id ?? '')
  const [projectFilter, setProjectFilter] = useState('')
  const pid = projectId || projects[0]?.id || ''
  const host = useStore((s) => projectHostName(s, pid || null))
  const tools = useStore((s) => toolsOfHost(s, host))
  const [kind, setKind] = useState<BotKind>(() => (pid ? defaultKindForProject(pid, tools, bots) : 'claude'))
  const [name, setName] = useState(() => defaultName || (pid ? uniqueBotName(kind, pid, bots) : ''))
  const [model, setModel] = useState<string | null>(null)
  const [effort, setEffort] = useState<string | null>(null)
  const [fast, setFast] = useState(false)
  const [persona, setPersona] = useState('')
  const [identity, setIdentity] = useState('')
  const [shareProfile, setShareProfile] = useState<ShareProfile | null>(null)
  const [shareFolder, setShareFolder] = useState<ShareFolderDraft>(EMPTY_SHARE_FOLDER)
  const [busy, setBusy] = useState(false)
  const nameRef = useRef<HTMLInputElement>(null)
  const nameTouched = useRef(Boolean(defaultName))
  const nameLabelId = useId()
  const nameHintId = useId()

  const nameOk = isValidBotName(name.trim())
  const apiKind = isApiKind(kind)
  const cliOk = apiKind || Boolean(tools[kind]?.installed)
  const defaultLoggedOut = !apiKind && cliOk && identity === '' && tools[kind]?.logged_in === false
  const shareOn = !apiKind && shareProfile !== null && !shareProfileBlocked(kind)
  const folderIn = shareOn ? shareFolderInput(shareFolder, name) : null
  const projectPath = projects.find((p) => p.id === pid)?.path
  const canSubmit = nameOk && cliOk && Boolean(pid) && !busy && !(folderIn && 'error' in folderIn)
  const hostUp = (h: string) => h === 'local' || (hosts.find((x) => x.name === h)?.connected ?? false)
  const filterQ = projectFilter.trim().toLowerCase()
  const visibleProjects = filterQ ? projects.filter((p) => p.label.toLowerCase().includes(filterQ)) : projects

  const [prevPid, setPrevPid] = useState(pid)
  if (!prevPid && pid) {
    setPrevPid(pid)
    const s = useStore.getState()
    const nextTools = toolsOfHost(s, projectHostName(s, pid))
    const currentBots = s.bots
    const nextKind = defaultKindForProject(pid, nextTools, currentBots)
    setKind(nextKind)
    if (!nameTouched.current) setName(uniqueBotName(nextKind, pid, currentBots))
  }

  const pickProject = (nextPid: string) => {
    setProjectId(nextPid)
    setPrevPid(nextPid)
    nameTouched.current = false
    const s = useStore.getState()
    const nextTools = toolsOfHost(s, projectHostName(s, nextPid))
    const currentBots = s.bots
    const nextKind = defaultKindForProject(nextPid, nextTools, currentBots)
    setKind(nextKind)
    setIdentity('')
    setModel(null)
    setEffort(null)
    setFast(false)
    setName(uniqueBotName(nextKind, nextPid, currentBots))
  }

  useEffect(() => {
    nameRef.current?.focus()
  }, [])

  const pickKind = (k: BotKind) => {
    setKind(k)
    setIdentity('')
    setModel(null)
    setEffort(null)
    setFast(false)
    if (!nameTouched.current && pid) setName(uniqueBotName(k, pid, bots))
  }

  if (projects.length === 0) {
    return <p className="hint">{t('請先新增一個 Project。')}</p>
  }

  const selectedProj = projects.find((p) => p.id === pid)
  if (selectedProj?.mode === 'expert' && !scheduler) {
    return (
      <div className="new-bot-dialog-expert-wrapper">
        {initialProjectId ? null : (
          <div className="field">
            <span>{t('Project')}</span>
            {projects.length > 6 ? (
              <input
                type="text"
                className="opt-filter"
                value={projectFilter}
                placeholder={t('過濾 Project…')}
                aria-label={t('過濾 Project')}
                spellCheck={false}
                onChange={(e) => setProjectFilter(e.target.value)}
              />
            ) : null}
            <div className="opt-group" role="radiogroup" aria-label="Project">
              {visibleProjects.map((p) => {
                const up = hostUp(p.host)
                return (
                  <button
                    key={p.id}
                    type="button"
                    role="radio"
                    aria-checked={pid === p.id}
                    className={`opt${pid === p.id ? ' on' : ''}`}
                    disabled={!up}
                    title={up ? p.path : t('{name}（主機未連線）', { name: p.label })}
                    onClick={() => pickProject(p.id)}
                  >
                    <span className="opt-label">{p.label}</span>
                    <HostBadge host={p.host} connected={up} />
                  </button>
                )
              })}
            </div>
            {visibleProjects.length === 0 ? <span className="hint">{t('沒有符合的 Project。')}</span> : null}
          </div>
        )}
        <NewExpertBotForm
          project={selectedProj}
          onDone={onDone}
          onCreated={onCreated}
        />
      </div>
    )
  }

  return (
    <form
      className="form"
      onSubmit={(e) => {
        e.preventDefault()
        if (!canSubmit) return
        setBusy(true)
        void addBot(pid, {
          name: name.trim(),
          kind,
          model,
          effort,
          fast: kind === 'codex' || apiKind ? fast : undefined,
          persona: apiKind ? null : persona.trim() || null,
          autostart: false,
          ...(scheduler ? { role: 'scheduler' as const } : {}),
          identity: apiKind ? null : identity || null,
          ...(apiKind ? {} : shareCreateFields(shareProfile, folderIn)),
        }).then(async (id) => {
          setBusy(false)
          if (id) {
            try {
              localStorage.setItem(lastKindKey(pid), kind)
            } catch {
            }
            onCreated?.(id)
            onDone()
            await startBot(id)
          }
        })
      }}
    >
      {initialProjectId ? null : (
        <div className="field">
          <span>{t('Project')}</span>
          {projects.length > 6 ? (
            <input
              type="text"
              className="opt-filter"
              value={projectFilter}
              placeholder={t('過濾 Project…')}
              aria-label={t('過濾 Project')}
              spellCheck={false}
              onChange={(e) => setProjectFilter(e.target.value)}
            />
          ) : null}
          <div className="opt-group" role="radiogroup" aria-label="Project">
            {visibleProjects.map((p) => {
              const up = hostUp(p.host)
              return (
                <button
                  key={p.id}
                  type="button"
                  role="radio"
                  aria-checked={pid === p.id}
                  className={`opt${pid === p.id ? ' on' : ''}`}
                  disabled={!up}
                  title={up ? p.path : t('{name}（主機未連線）', { name: p.label })}
                  onClick={() => pickProject(p.id)}
                >
                  <span className="opt-label">{p.label}</span>
                  <HostBadge host={p.host} connected={up} />
                </button>
              )
            })}
          </div>
          {visibleProjects.length === 0 ? <span className="hint">{t('沒有符合的 Project。')}</span> : null}
        </div>
      )}
      <div className="field">
        <span>Kind</span>
        <div className="opt-group kinds" role="radiogroup" aria-label="kind">
          {CREATABLE_KINDS.map((k) => {
            const missing = isApiKind(k) ? false : !tools[k].installed
            const reason = missing ? t('{host} 尚未安裝 {kind}', { host: host === 'local' ? t('本機') : host, kind: k }) : k
            return (
              <span key={k} className="opt-wrap">
                <button
                  type="button"
                  role="radio"
                  aria-checked={kind === k}
                  className={`opt${kind === k ? ' on' : ''}`}
                  disabled={missing}
                  title={reason}
                  onClick={() => pickKind(k)}
                >
                  <KindTag kind={k} />
                  <span className="opt-label">{k}</span>
                </button>
                {missing ? (
                  <>
                    <span className="kind-missing-reason">{t('未安裝')}</span>
                    <InstallToolButton host={host} kind={k} small />
                  </>
                ) : null}
              </span>
            )
          })}
        </div>
      </div>
      <ApiModelFields
        kind={kind}
        host={host}
        identity={identity || null}
        model={model}
        onModel={setModel}
        effort={effort}
        onEffort={setEffort}
        fast={fast}
        onFast={setFast}
      />
      {apiKind ? null : <IdentityOptions kind={kind} host={host} value={identity} onChange={setIdentity} />}
      {defaultLoggedOut ? (
        <span className="hint identity-selection-warning" role="status">
          {t('{host} 上的 {kind} 預設帳號還沒登入：bot 起來會停在登入畫面。先登入，或在上面選一個已登入的身份。', { host: host === 'local' ? t('本機') : host, kind })}
          <button type="button" className="btn" onClick={() => void loginKindOnHost(host, kind, null)}>
            {t('登入 {kind}', { kind })}
          </button>
        </span>
      ) : null}
      <label className="field">
        <span id={nameLabelId}>{t('名稱')}</span>
        <input
          ref={nameRef}
          type="text"
          aria-labelledby={nameLabelId}
          aria-invalid={name && !nameOk ? true : undefined}
          aria-describedby={name && !nameOk ? nameHintId : undefined}
          value={name}
          placeholder={`${kind}-1`}
          spellCheck={false}
          onChange={(e) => {
            nameTouched.current = true
            setName(e.target.value)
          }}
        />
        {name && !nameOk ? <span id={nameHintId} className="hint">{t.tr(BOT_NAME_HINT)}</span> : null}
        {!cliOk ? <span className="hint">{t('此 kind 的 CLI 尚未安裝，無法建立')}</span> : null}
      </label>
      {apiKind ? null : <PersonaField value={persona} onChange={setPersona} collapsible />}
      {apiKind || scheduler ? null : <ShareProfileField kind={kind} host={host} value={shareProfile} onChange={setShareProfile} botName={name.trim()} folder={shareFolder} onFolder={setShareFolder} projectPath={projectPath} />}
      <div className="form-actions">
        <button type="button" className="btn" onClick={onDone}>
          {t('取消')}
        </button>
        <button type="submit" className="btn primary" disabled={!canSubmit}>
          {busy ? t('建立中…') : t('新增並啟動')}
        </button>
      </div>
    </form>
  )
}

function SchedulerBotSection({ count, shut, forceOpen, attention, projectLabel, onToggle, children }: { count: number; shut: boolean; forceOpen: boolean; attention: 'blocked' | 'unread' | null; projectLabel: string; onToggle: () => void; children: ReactNode }) {
  const t = useT()
  const shown = !shut || forceOpen
  return (
    <div className="scheduler-bots">
      <button type="button" className="scheduler-bots-head" aria-expanded={shown} onClick={onToggle}>
        <span aria-hidden="true">{shown ? '▾' : '▸'}</span> {t('排程 bot')} <span className="scheduler-bots-count">{count}</span>
        {!shown && attention ? (
          <span className={`scheduler-bots-attn ${attention}`} title={attention === 'blocked' ? t('有排程 bot 卡住了，點開看') : t('有排程 bot 的回合還沒看')}>
            {attention === 'blocked' ? t('⚠ 需處理') : t('● 有未讀')}
          </span>
        ) : null}
      </button>
      {shown ? (
        <div className="bot-list" role="list" aria-label={t('{name} 的排程 bot', { name: projectLabel })}>
          {children}
        </div>
      ) : null}
    </div>
  )
}

function useBotScheduleCodes(projectId: string, enabled: boolean): Map<string, string[]> {
  const rev = useStore((s) => s.schedulesRev)
  const bots = useStore((s) => s.bots)
  const [map, setMap] = useState<Map<string, string[]>>(new Map())
  const seq = useRef(0)
  const refresh = useCallback(() => {
    const id = ++seq.current
    listSchedules(projectId)
      .then((rows) => {
        if (seq.current !== id) return
        const m = new Map<string, string[]>()
        for (const r of rows) m.set(r.bot_id, [...(m.get(r.bot_id) ?? []), r.code || r.name])
        setMap(m)
      })
      .catch(() => {})
  }, [projectId])
  useEffect(() => {
    if (!enabled) return
    refresh()
    return () => {
      seq.current += 1
    }
  }, [enabled, refresh, rev, bots])
  useEffect(() => {
    if (!enabled) return
    const onChanged = (ev: Event) => {
      const detail = (ev as CustomEvent<{ project_id?: string }>).detail
      if (!detail?.project_id || detail.project_id === projectId) refresh()
    }
    window.addEventListener('agm:schedules_changed', onChanged)
    return () => window.removeEventListener('agm:schedules_changed', onChanged)
  }, [enabled, projectId, refresh])
  return map
}

function SchedulerBotRows({ projectId, bots, render }: { projectId: string; bots: Bot[]; render: (b: Bot) => ReactNode }) {
  const t = useT()
  const codes = useBotScheduleCodes(projectId, true)
  return (
    <>
      {bots.map((b) => (
        <Fragment key={b.id}>
          {render(b)}
          {codes.get(b.id)?.length ? (
            <div className="scheduler-bot-codes" title={t('這顆 bot 服務的排程')}>
              <ClockIcon /> {codes.get(b.id)!.join(' ')}
            </div>
          ) : null}
        </Fragment>
      ))}
    </>
  )
}

function ProjectTitle({
  projectId,
  label,
  host,
  path,
  hostUp,
  folded,
  draggable,
}: {
  projectId: string
  label: string
  host: string
  path: string
  hostUp: boolean
  draggable?: boolean
  folded?: boolean
}) {
  useT()
  const selected = useStore((s) => s.selectedProjectId === projectId)
  const foldedUnread = useStore((s) => (folded ? projectUnread(s, projectId) : 0))
  const isExpert = useStore((s) => s.projects.find((p) => p.id === projectId)?.mode === 'expert')
  const selectProject = useStore((s) => s.selectProject)
  const [editing, setEditing] = useState(false)
  const inner = (
    <>
      <span className="project-group-icon" aria-hidden="true">
        ⌗
      </span>
      <ProjectNameField projectId={projectId} label={label} variant="row" armed={false} editing={editing} onEditing={setEditing} />
      <span className="project-badges">
        {isExpert ? <ExpertBadge /> : null}
        {foldedUnread > 0 ? (
          <span className="unread-turns" title={t('收合的 Bot 裡有 {n} 個回合已完成，還沒看過', { n: foldedUnread })}>
            !{foldedUnread > 99 ? '99+' : foldedUnread}
          </span>
        ) : null}
        <HostBadge host={host} connected={hostUp} />
        <HandedOffBadge projectId={projectId} />
        <ProjectMemBadge projectId={projectId} />
      </span>
      <span className="project-path" title={path}>
        {shortPath(path, 36)}
      </span>
    </>
  )
  if (editing) {
    return <div className={`project-label-btn${selected ? ' selected' : ''}`}>{inner}</div>
  }
  return (
    <button
      type="button"
      className={`project-label-btn${selected ? ' selected' : ''}`}
      title={t('開啟「{name}」的專案設定\n{path}{drag}', { name: label, path, drag: draggable ? t('\n拖曳可調整專案順序') : '' })}
      aria-current={selected ? 'true' : undefined}
      onClick={(e) => {
        e.stopPropagation()
        selectProject(projectId)
        requestAnimationFrame(() => scrollProjectIntoView(projectId))
      }}
    >
      {inner}
    </button>
  )
}

export type KidsWait = 'busy' | 'reply'

function kidsWaitOf(st: Parameters<typeof botLamp>[0], unread: Record<string, number>, ids: string[]): KidsWait | null {
  let busy = false
  let reply = false
  for (const id of ids) {
    const l = botLamp(st, id)
    if (l === 'blocked') return 'busy'
    if (l === 'working') busy = true
    if ((unread[id] ?? 0) > 0) reply = true
  }
  return reply ? 'reply' : busy ? 'busy' : null
}

function kidsLampOf(st: Parameters<typeof botLamp>[0], ids: string[]): Lamp | null {
  let out: Lamp | null = null
  for (const id of ids) {
    const l = botLamp(st, id)
    if (l === 'blocked') return 'blocked'
    if (l === 'working') out = 'working'
  }
  return out
}

function kidsWheel(e: WheelEvent) {
  const el = e.currentTarget
  if (!(el instanceof HTMLElement)) return
  wheelKidsScroll({ currentTarget: el, deltaX: e.deltaX, deltaY: e.deltaY, shiftKey: e.shiftKey, preventDefault: () => e.preventDefault() })
}

type ProjectDrag = { id: string; overId: string | null; edge: 'before' | 'after' } | null

export function Sidebar() {
  const t = useT()
  const phone = useMediaQuery(PHONE_QUERY)
  const rawProjects = useStore((s) => s.projects)
  const projectOrder = useStore((s) => s.projectOrder)
  const moveProject = useStore((s) => s.moveProject)
  const projects = useMemo(() => orderedProjects({ projects: rawProjects, projectOrder }), [rawProjects, projectOrder])
  const [pdrag, setPdrag] = useState<ProjectDrag>(null)
  const projectEdgeAt = (e: { currentTarget: HTMLElement; clientY: number }): 'before' | 'after' => {
    const r = e.currentTarget.getBoundingClientRect()
    return e.clientY < r.top + r.height / 2 ? 'before' : 'after'
  }
  const dropProjectAt = (dragId: string, overId: string, edge: 'before' | 'after') => {
    const ids = projects.map((p) => p.id)
    const at = ids.indexOf(overId)
    if (at < 0) return
    const beforeId = edge === 'before' ? overId : (ids[at + 1] ?? null)
    if (beforeId === dragId) return
    moveProject(dragId, beforeId)
  }
  const bots = useStore((s) => s.bots)
  const hosts = useStore((s) => s.hosts)
  const socket = useStore((s) => s.socket)
  const connected = useStore((s) => s.connected)
  const selectedProjectId = useStore((s) => s.selectedProjectId)
  const selectProject = useStore((s) => s.selectProject)
  const removeProject = useStore((s) => s.removeProject)
  const [open, setOpen] = useState<'project' | 'env' | null>(null)
  const configuredIdentities = useStore((s) => s.identities)
  const localIdentityStatus = useStore((s) => s.localIdentityStatus)
  const disabledIdentities = useStore((s) => s.disabledIdentities)
  const identityCount = useMemo(
    () => enabledIdentities(disabledIdentities, 'local', identitiesOfHost(configuredIdentities, localIdentityStatus, 'local')).length,
    [configuredIdentities, localIdentityStatus, disabledIdentities],
  )
  const [deleteProject, setDeleteProject] = useState<{ id: string; label: string } | null>(null)
  const runs = useStore((s) => s.runs)
  const defaultConnected = useStore((s) => s.defaultConnected)
  const lampState = useMemo(
    () => ({ ...useStore.getState(), runs, hosts, connected, defaultConnected, bots, projects: rawProjects }),
    [runs, hosts, connected, defaultConnected, bots, rawProjects],
  )
  const unreadMap = useStore((s) => s.botUnread)
  const deleteTarget = deleteProject ? projects.find((p) => p.id === deleteProject.id) : undefined
  const deleteBlockers = deleteProject ? projectDeleteBlockers(bots, runs, deleteProject.id) : { total: 0, active: 0 }
  const [botFormFor, setBotFormFor] = useState<string | null>(null)
  const [scheduleEditor, setScheduleEditor] = useState<{ projectId: string; scheduleId: string | null; focusBot?: boolean } | null>(null)
  useEffect(() => {
    const open = (target: { projectId: string; scheduleId: string } | null) => {
      if (target) setScheduleEditor({ projectId: target.projectId, scheduleId: target.scheduleId })
    }
    const initial = parseScheduleUrl(window.location.href)
    if (initial) {
      open(initial)
      try {
        window.history.replaceState(null, '', window.location.pathname)
      } catch {
      }
    }
    const onOpen = (ev: Event) => open((ev as CustomEvent<{ projectId: string; scheduleId: string }>).detail)
    window.addEventListener('agm:open-schedule', onOpen)
    return () => window.removeEventListener('agm:open-schedule', onOpen)
  }, [])
  const [botSheetOpen, setBotSheetOpen] = useState(false)
  const openBotSheetFor = useStore((s) => s.openBotSheetFor)
  const clearOpenBotSheet = useStore((s) => s.clearOpenBotSheet)
  const botOrder = useStore((s) => s.botOrder)
  const moveBot = useStore((s) => s.moveBot)
  const kidsWheelRef = useWheelRef<HTMLDivElement>(kidsWheel)
  const [drag, setDrag] = useState<DragState>(null)
  const openHostShell = useStore((s) => s.openHostShell)
  const hiddenQuotaIds = useStore(useShallow((s) => s.hiddenBotIds))
  const hiddenQuota = useMemo(() => new Set(hiddenQuotaIds), [hiddenQuotaIds])
  const [collapsed, setCollapsed] = useState<Set<string>>(() => {
    try {
      const raw = localStorage.getItem('am.collapsedChildren')
      return new Set(raw ? (JSON.parse(raw) as string[]) : [])
    } catch {
      return new Set()
    }
  })
  const toggleChildren = useStableCallback((id: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev)
      if (!next.delete(id)) next.add(id)
      try {
        persistSetDiff('am.collapsedChildren', prev, next)
      } catch {
      }
      return next
    }),
  )
  const [shutProjects, setShutProjects] = useState<Set<string>>(() => {
    try {
      const raw = localStorage.getItem('am.collapsedProjects')
      return new Set(raw ? (JSON.parse(raw) as string[]) : [])
    } catch {
      return new Set()
    }
  })
  const selectedBotId = useStore((s) => s.selectedBotId)
  const selectedBotProject = useStore((s) => s.bots.find((b) => b.id === s.selectedBotId)?.project_id ?? null)
  useEffect(() => {
    if (!selectedBotId) return
    if (selectedBotProject && shutProjects.has(selectedBotProject)) {
      setShutProjects((prev) => {
        if (!prev.has(selectedBotProject)) return prev
        const next = new Set(prev)
        next.delete(selectedBotProject)
        try {
          persistSetDiff('am.collapsedProjects', prev, next)
        } catch {
        }
        return next
      })
    }
    const toTop = botTopPending(selectedBotId)
    const id = requestAnimationFrame(() => {
      if (toTop || botTopPending(selectedBotId)) return
      document.querySelector<HTMLElement>(`[data-bot-id="${selectedBotId}"]`)?.scrollIntoView({ block: 'nearest' })
    })
    return () => cancelAnimationFrame(id)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedBotId, selectedBotProject])
  useBotScrollTop(setShutProjects, setCollapsed)
  const [shutSched, setShutSched] = useState<Set<string>>(() => {
    try {
      const raw = localStorage.getItem('am.collapsedSchedBots')
      return new Set(raw ? (JSON.parse(raw) as string[]) : [])
    } catch {
      return new Set()
    }
  })
  const toggleSched = (id: string) =>
    setShutSched((prev) => {
      const next = new Set(prev)
      if (!next.delete(id)) next.add(id)
      try {
        persistSetDiff('am.collapsedSchedBots', prev, next)
      } catch {
      }
      return next
    })
  const toggleProject = (id: string) =>
    setShutProjects((prev) => {
      const next = new Set(prev)
      if (!next.delete(id)) next.add(id)
      try {
        persistSetDiff('am.collapsedProjects', prev, next)
      } catch {
      }
      return next
    })
  const [query, setQuery] = useState('')
  const [hits, setHits] = useState<Record<string, MessageHit>>({})
  const hitSearch = useMemo(() => createHitSearch(api.searchMessages, (r) => setHits(r ?? {})), [])
  useEffect(() => {
    hitSearch.run(query)
    return () => hitSearch.cancel()
  }, [query, hitSearch])
  const matches = (bot: Bot) => botMatches(useStore.getState(), bot, query) || bot.id in hits
  const matchCount = query ? bots.filter((b) => matches(b)).length : bots.length
  const hitCount = bots.filter((b) => b.id in hits).length

  const dropAt = useStableCallback((dragId: string, overId: string, edge: 'before' | 'after') => {
    const bot = bots.find((b) => b.id === overId)
    const dragItem = bots.find((b) => b.id === dragId)
    if (!bot || !dragItem) return
    const proj = useStore.getState().projects.find((p) => p.id === bot.project_id)
    if (proj?.mode === 'expert' && !canReorderInExpertMode(dragItem, bot)) return
    const ids = botsOfProject({ bots, botOrder }, bot.project_id).map((b) => b.id)
    const at = ids.indexOf(overId)
    if (at < 0) return
    const beforeId = edge === 'before' ? overId : (ids[at + 1] ?? null)
    if (beforeId === dragId) return
    moveBot(dragId, beforeId)
  })

  const nudge = useStableCallback((botId: string, dir: -1 | 1) => {
    const bot = bots.find((b) => b.id === botId)
    if (!bot || bot.parent_bot_id) return
    const proj = useStore.getState().projects.find((p) => p.id === bot.project_id)
    const allParentBots = botsOfProject({ bots, botOrder }, bot.project_id).filter((b) => !b.parent_bot_id)
    const ids = (proj?.mode === 'expert'
      ? allParentBots.filter((b) => canReorderInExpertMode(bot, b))
      : allParentBots
    ).map((b) => b.id)
    const at = ids.indexOf(botId)
    const to = at + dir
    if (at < 0 || to < 0 || to >= ids.length) return
    moveBot(botId, dir === -1 ? ids[to] : (ids[to + 1] ?? null))
  })

  const step = useStableCallback((botId: string, dir: -1 | 1) => {
    const st = useStore.getState()
    const offscreen = (id: string) => {
      if (query) return !document.querySelector(`[data-bot-id="${id}"]`)
      let parent = st.bots.find((x) => x.id === id)?.parent_bot_id
      for (let i = 0; parent && i < st.bots.length; i += 1) {
        if (collapsed.has(parent)) return true
        parent = st.bots.find((x) => x.id === parent)?.parent_bot_id
      }
      return false
    }
    let next = adjacentBotId(st, botId, dir)
    for (let i = 0; next && next !== botId && offscreen(next) && i < st.bots.length; i += 1) next = adjacentBotId(st, next, dir)
    if (!next || next === botId || offscreen(next)) return
    st.selectBot(next)
    requestAnimationFrame(() => {
      const el = document.querySelector<HTMLElement>(`[data-bot-id="${next}"]`)
      el?.focus()
      el?.scrollIntoView({ block: 'nearest' })
    })
  })

  const rowShared = { drag, onDrag: setDrag, onDropAt: dropAt, onNudge: nudge, onStep: step }

  const hostUp = (name: string) => name === 'local' || (hosts.find((h) => h.name === name)?.connected ?? false)
  const hostsDown = hosts.filter((h) => !h.connected).length

  const closeBotSheet = () => {
    setBotFormFor(null)
    setBotSheetOpen(false)
  }

  const openBotSheet = (projectId?: string) => {
    setOpen(null)
    setBotFormFor(projectId ?? null)
    setBotSheetOpen(true)
  }

  useEffect(() => {
    if (!openBotSheetFor) return
    openBotSheet(openBotSheetFor)
    clearOpenBotSheet()
  }, [openBotSheetFor, clearOpenBotSheet])

  useEffect(() => {
    const onAdd = () => setOpen('project')
    window.addEventListener(ADD_PROJECT_EVENT, onAdd)
    return () => window.removeEventListener(ADD_PROJECT_EVENT, onAdd)
  }, [])

  const botSheetProject = botFormFor ? projects.find((p) => p.id === botFormFor) : null
  const botSheetLabel = botSheetProject?.label ?? t('選擇 Project')

  return (
    <>
      <span id={BOT_ROW_HINT_ID} className="sr-only">
        {t('Bot 順序可以拖曳調整，鍵盤按 Alt 加上下方向鍵移動一格。')}
      </span>
      <div className="sidebar-head">
        <div className="head-brand">
          <div className="head-brand-title">
            <h1 title="Agents Manager">AG Man</h1>
            {MOCK_MODE ? <span className="mock-badge">MOCK</span> : null}
            {}
            <span className="head-deploy">
              {}
              <DeployNowBadge />
              {}
              <DeployWaitChip />
            </span>
            {}
            {phone ? <UpdateQuotaChip /> : null}
          </div>
          <div className="head-brand-meta">
            <PaneBadge />
            <ThemeToggle />
            <ConnBadge socket={socket} connected={connected} />
          </div>
        </div>
        <HeadMemBadges />
        <div className="head-right">
          <TabsBadge />
        </div>
      </div>

      {}
      <UpdateAllBanner />

      {}
      <div className="bot-search">
        <input
          type="search"
          className="bot-search-input"
          value={query}
          spellCheck={false}
          placeholder={phone ? t('搜尋 bot…') : t('搜尋 bot…（名稱、專案、模型、人設）')}
          aria-label={t('搜尋 bot')}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Escape') {
              e.stopPropagation()
              e.preventDefault()
              setQuery('')
            }
          }}
        />
        {query ? (
          <span className="bot-search-count" title={hitCount ? t('其中 {n} 個是對話內容命中', { n: hitCount }) : undefined}>
            {t('{n} 個符合', { n: matchCount })}
            {hitCount ? <span className="bot-search-hits">{t('（{n} 個含對話）', { n: hitCount })}</span> : null}
            <button type="button" className="bot-search-clear" title={t('清除搜尋（Esc）')} onClick={() => setQuery('')}>
              ✕
            </button>
          </span>
        ) : null}
      </div>

      {}
      <nav className="sidebar-scroll" aria-label={t('Bot 清單')}>
        {query && matchCount === 0 ? (
          <p className="hint" style={{ padding: '12px 14px' }}>
            {t('沒有符合「{query}」的 Bot。', { query })}
          </p>
        ) : null}
        {}
        {projects.length === 0 && socket !== 'open' ? (
          <p className="hint" style={{ padding: '12px 14px' }}>
            {t('正在連線 daemon，稍等一下就會列出 Project…')}
          </p>
        ) : projects.length === 0 ? (
          <p className="hint" style={{ padding: '12px 14px' }}>
            {t('尚未設定任何 Project。請用下方的「新增 Project」開始。')}
          </p>
        ) : null}
        {projects.map((p) => {
          const every = botsOfProject({ bots, botOrder }, p.id)
          const hit = new Set(every.filter((b) => matches(b)).map((b) => b.id))
          const all = every.filter(
            (b) =>
              hit.has(b.id) ||
              (b.parent_bot_id ? hit.has(b.parent_bot_id) : every.some((c) => c.parent_bot_id === b.id && hit.has(c.id))),
          )
          const hiddenBots = query ? [] : all.filter((b) => hiddenQuota.has(b.id))
          const hiddenCount = hiddenBots.length
          const hiddenIdentities = hiddenCount > 0
            ? [...new Set(hiddenBots.map((b) => quotaIdentityLabel(b.kind, b.identity)))]
                .sort((a, b) => a.localeCompare(b, undefined, { numeric: true }))
                .join(t('、'))
            : ''
          const shown = query ? all : all.filter((b) => !hiddenQuota.has(b.id))
          const shownIds = new Set(shown.map((b) => b.id))
          const topLevel = shown.filter((b) => !b.parent_bot_id || !shownIds.has(b.parent_bot_id))
          const list = topLevel.filter((b) => b.role !== 'scheduler')
          const schedBots = topLevel.filter((b) => b.role === 'scheduler')
          const childrenOf = (id: string) => shown.filter((b) => b.parent_bot_id === id)
          const descendantsOf = (id: string, seen = new Set<string>()): Bot[] =>
            childrenOf(id).flatMap((c) => (seen.has(c.id) ? [] : (seen.add(c.id), [c, ...descendantsOf(c.id, seen)])))
          const projectSelected = selectedProjectId === p.id
          const projectShut = shutProjects.has(p.id) && !query
          if (query && topLevel.length === 0) return null
          const pDragging = pdrag?.id === p.id
          const pDropEdge = pdrag && pdrag.id !== p.id && pdrag.overId === p.id ? pdrag.edge : null
          const renderMember = (b: Bot) => {
            const kids = descendantsOf(b.id)
            const shut = kids.length > 0 && collapsed.has(b.id) && !query
            const kidsLamp = shut ? kidsLampOf(lampState, kids.map((c) => c.id)) : null
            const kidsWait = kids.length ? kidsWaitOf(lampState, unreadMap, kids.map((c) => c.id)) : null
            return (
              <Fragment key={b.id}>
                <BotRow
                  botId={b.id}
                  hit={hits[b.id]}
                  childCount={kids.length}
                  collapsed={shut}
                  kidsLamp={kidsLamp}
                  kidsWait={kidsWait}
                  kidsListId={shut || kids.length === 0 ? undefined : `bot-kids-${b.id}`}
                  onToggleChildren={toggleChildren}
                  {...rowShared}
                />
                {shut || kids.length === 0 ? null : (
                  <div id={`bot-kids-${b.id}`} className="bot-kids" role="list" aria-label={t('{name} 的 {n} 個子 agent', { name: b.name, n: kids.length })} ref={kidsWheelRef}>
                    {kids.map((c, i) => (
                      <div key={c.id} className={`bot-child${i === kids.length - 1 ? ' last' : ''}${c.parent_bot_id !== b.id ? ' deep' : ''}`}>
                        <BotRow compact botId={c.id} hit={hits[c.id]} {...rowShared} />
                      </div>
                    ))}
                  </div>
                )}
              </Fragment>
            )
          }
          return (
            <section
              className={`project${pDragging ? ' dragging' : ''}${pDropEdge ? ` drop-${pDropEdge}` : ''}${hostUp(p.host) ? '' : ' host-down'}`}
              key={p.id}
              data-project-id={p.id}
              onDragOver={(e) => {
                if (!pdrag || pdrag.id === p.id) return
                e.preventDefault()
                e.dataTransfer.dropEffect = 'move'
                const edge = projectEdgeAt(e)
                if (pdrag.overId !== p.id || pdrag.edge !== edge) setPdrag({ ...pdrag, overId: p.id, edge })
              }}
              onDrop={(e) => {
                if (!pdrag || pdrag.id === p.id) return
                e.preventDefault()
                dropProjectAt(pdrag.id, p.id, projectEdgeAt(e))
                setPdrag(null)
              }}
            >
              <header
                className={`project-head${projectSelected ? ' selected' : ''}`}
                onClick={() => {
                  selectProject(p.id)
                  requestAnimationFrame(() => scrollProjectIntoView(p.id))
                }}
                draggable={!query}
                onDragStart={(e) => {
                  e.stopPropagation()
                  e.dataTransfer.effectAllowed = 'move'
                  e.dataTransfer.setData('text/plain', `project:${p.id}`)
                  setPdrag({ id: p.id, overId: null, edge: 'before' })
                }}
                onDragEnd={() => setPdrag(null)}
              >
                {}
                <button
                  type="button"
                  className={`project-fold${projectShut ? ' shut' : ''}`}
                  aria-expanded={!projectShut}
                  title={projectShut ? t('展開 {name}（{n} 個 Bot）', { name: p.label, n: all.length }) : t('收合 {name}', { name: p.label })}
                  onClick={(e) => {
                    e.stopPropagation()
                    toggleProject(p.id)
                  }}
                >
                  <span className="chev">{projectShut ? '▶' : '▼'}</span>
                </button>
                <ProjectTitle projectId={p.id} label={p.label} host={p.host} path={p.path} hostUp={hostUp(p.host)} folded={projectShut} draggable={!query} />
                {}
                <span className="project-head-actions" onClick={(e) => e.stopPropagation()}>
                  <ProjectAddMenu label={p.label} onBot={() => openBotSheet(p.id)} onSchedule={() => setScheduleEditor({ projectId: p.id, scheduleId: null })} />
                  {}
                  <HeadMoreMenu label={t('更多動作 · {name}', { name: p.label })}>
                    <button
                      type="button"
                      className="head-menu-item"
                      role="menuitem"
                      disabled={!hostUp(p.host)}
                      title={
                        hostUp(p.host)
                          ? t('在 {host} 的 {path} 開一個 shell', { host: p.host === 'local' ? t('本機') : p.host, path: p.path })
                          : t('主機未連線，開不了 shell')
                      }
                      onClick={(e) => {
                        e.stopPropagation()
                        void openHostShell(p.host, p.path)
                      }}
                    >
                      {t('在這裡開 shell')}
                    </button>
                    <button
                      type="button"
                      className="head-menu-item danger"
                      role="menuitem"
                      onClick={(e) => {
                        e.stopPropagation()
                        setDeleteProject({ id: p.id, label: p.label })
                      }}
                    >
                      {t('刪除專案…')}
                    </button>
                  </HeadMoreMenu>
                </span>
              </header>
              {projectShut ? (
                <button type="button" className="project-folded" onClick={() => toggleProject(p.id)}>
                  {t('{n} 個 Bot·點一下展開', { n: all.length })}
                </button>
              ) : topLevel.length === 0 && hiddenCount > 0 ? (
                <div className="project-empty">
                  <span className="project-quota-hidden">{t('{n} 個 Bot 已隱藏（{identities} 已停用）', { n: hiddenCount, identities: hiddenIdentities })}</span>
                  <QuickAddBots projectId={p.id} />
                </div>
              ) : topLevel.length === 0 ? (
                <div className="project-empty">
                  <span className="project-empty-title">{t('此專案尚無 Bot')}</span>
                  <QuickAddBots projectId={p.id} />
                </div>
              ) : (
                <div className="bot-list" role="list" aria-label={t('{name} 的 Bot', { name: p.label })}>
                {p.mode === 'expert' ? (
                  groupExpertSections(list, p.deputy?.bot_id).map((sec) =>
                    sec.role === 'executor' ? (
                      <ExpertExecutorSection key={sec.role} projectId={p.id} title={sec.title} abbr={sec.abbr} items={sec.items} query={query} renderMember={renderMember} />
                    ) : (
                      <Fragment key={sec.role}>
                        <ExpertSectionHeader title={sec.title} abbr={sec.abbr} />
                        {sec.items.map((item) => renderMember(item))}
                      </Fragment>
                    ),
                  )
                ) : (
                  list.map(renderMember)
                )}
                </div>
              )}
              {}
              {projectShut || schedBots.length === 0 ? null : (
                <SchedulerBotSection
                  count={schedBots.length}
                  shut={shutSched.has(p.id)}
                  forceOpen={Boolean(query)}
                  attention={
                    schedBots.some((b) => botLamp(lampState, b.id) === 'blocked') ? 'blocked' : schedBots.some((b) => (unreadMap[b.id] ?? 0) > 0) ? 'unread' : null
                  }
                  projectLabel={p.label}
                  onToggle={() => toggleSched(p.id)}
                >
                  <SchedulerBotRows
                    projectId={p.id}
                    bots={schedBots}
                    render={(b) => <BotRow botId={b.id} hit={hits[b.id]} {...rowShared} />}
                  />
                </SchedulerBotSection>
              )}
              {projectShut ? null : <ProjectSchedules projectId={p.id} onOpen={(sid, focusBot) => setScheduleEditor({ projectId: p.id, scheduleId: sid, focusBot })} />}
              {projectShut ? null : <SidebarPanes projectId={p.id} />}
              {}
              {!projectShut && list.length > 0 && hiddenCount > 0 ? (
                <p className="project-quota-hidden">{t('{n} 個 Bot 已隱藏（{identities} 已停用）', { n: hiddenCount, identities: hiddenIdentities })}</p>
              ) : null}
            </section>
          )
        })}
      </nav>

      <div className="sidebar-foot">
        <div className="sidebar-foot-actions">
          <button type="button" className="btn" onClick={() => setOpen('project')}>
            {t('新增 Project')}
          </button>
          <button type="button" className="btn" onClick={() => openBotSheet(selectedProjectId ?? selectedBotProject ?? projects[0]?.id)} disabled={projects.length === 0}>
            {t('新增 Bot')}
          </button>
        </div>

        <button
          type="button"
          className="disclosure"
          title={t('在本機開一個 shell，直接下指令')}
          onClick={() => void openHostShell(LOCAL_HOST)}
        >
          <TerminalIcon /> {t('開 shell')}
          <span className="disclosure-note">{t('本機')}</span>
        </button>
        {}
        <SidebarUnownedPanes />

        <button
          type="button"
          className="disclosure"
          aria-haspopup="dialog"
          aria-expanded={open === 'env'}
          onClick={() => setOpen('env')}
        >
          <GearIcon /> {t('環境設定')}
          <span className="disclosure-note">
            {hosts.length === 0 ? t('本機') : t('本機 + {n}', { n: hosts.length })}
            {hostsDown > 0 ? t('・ {n} 未連線', { n: hostsDown }) : ''}
            {t('・ 身份 {n}', { n: identityCount })}
          </span>
        </button>
      </div>

      <Modal open={open === 'project'} title={t('新增 Project')} onClose={() => setOpen(null)}>
        <NewProjectForm onDone={() => setOpen(null)} />
      </Modal>

      <Modal
        open={botSheetOpen}
        title={t('新增 Bot')}
        subtitle={<span title={botSheetProject?.path}>{botSheetLabel}</span>}
        onClose={closeBotSheet}
      >
        <NewBotForm key={botFormFor ?? 'pick'} initialProjectId={botFormFor ?? undefined} onDone={closeBotSheet} />
      </Modal>

      {scheduleEditor ? (
        <ScheduleEditor
          projectId={scheduleEditor.projectId}
          scheduleId={scheduleEditor.scheduleId}
          focusBot={scheduleEditor.focusBot}
          onClose={() => setScheduleEditor(null)}
          onSelect={(id) => setScheduleEditor((cur) => (cur ? { ...cur, scheduleId: id } : cur))}
        />
      ) : null}

      <ConfirmDialog
        open={deleteProject !== null}
        title={t('刪除 Project')}
        body={
          <>
            {t.x('要把 {name}{target} 從清單移除嗎？磁碟上的目錄與其中的檔案都不會動{children}。', { name: <strong>{deleteProject?.label}</strong>, target: deleteTarget ? t.x('（{host} {path}）', { host: deleteTarget.host === LOCAL_HOST ? t('本機') : deleteTarget.host, path: <code>{deleteTarget.path}</code> }) : '', children: deleteBlockers.total > 0 ? t('，底下 {n} 個 Bot 的設定會一起移除', { n: deleteBlockers.total }) : '' })}
            {deleteBlockers.active > 0 ? (
              <>
                <br />
                <strong>{t('仍有 {n} 個 Bot 在跑', { n: deleteBlockers.active })}</strong>{t('，需先停止才能刪除。')}
              </>
            ) : null}
          </>
        }
        confirmLabel={t('刪除 Project')}
        confirmDisabled={deleteBlockers.active > 0}
        danger
        onCancel={() => setDeleteProject(null)}
        onConfirm={() => {
          const id = deleteProject?.id
          setDeleteProject(null)
          if (id) void removeProject(id)
        }}
      />

      <Modal open={open === 'env'} title={t('環境設定')} width={960} onClose={() => setOpen(null)}>
        <EnvShell />
      </Modal>
    </>
  )
}
