import { useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import { useReviewAsk } from '../hooks/useReviewAsk'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import { updateBatchCounts } from '../lib/updateBatch'
import { updateRange } from '../lib/updateRange'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'
import { AgmReviewBox } from './AgmReviewBox'
import { HerdrUpdateChip } from './HerdrUpdateChip'
import { GrokUpdateChip } from './GrokUpdateChip'
import { AgyUpdateChip } from './AgyUpdateChip'
import { useHerdrPlan } from '../hooks/useHerdrPlan'
import { useGrokPlan } from '../hooks/useGrokPlan'
import { useAgyPlan } from '../hooks/useAgyPlan'
import { useHerdrUpdate } from '../store/herdrUpdate'
import { MergedUpdateChip, type DialogControl } from './MergedUpdateChip'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { mergeUpdateChips } from '../lib/updateBatch'
import { useInstallTodo } from '../hooks/useInstallTodo'
import { useUpdateChipRequest } from '../hooks/useUpdateChipRequest'
import { CLI_UPDATE_PHASE_LABEL } from '../store/cliUpdate'
import './updateQuotaChip.css'

export function UpdateQuotaChip() {
  const phone = useMediaQuery(PHONE_QUERY)
  const installShown = useInstallTodo()
  const restartShown = useStore((s) => {
    if (s.restartBatch) return true
    const c = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
    return c.ready.length > 0 || c.busy.some((b) => !b.install)
  }) || installShown.count > 0 || installShown.active.length > 0
  const herdrPlan = useHerdrPlan() !== null
  const herdrShown = useHerdrUpdate((s) => s.active !== null || s.result !== null) || herdrPlan
  const grokShown = useGrokPlan() !== null
  const agyShown = useAgyPlan() !== null
  if (mergeUpdateChips(phone, restartShown, false, false, herdrShown, grokShown, agyShown)) return <MergedUpdateChip />
  return (
    <>
      <RestartChip />
      <HerdrUpdateChip />
      {}
      <GrokUpdateChip />
      {}
      <AgyUpdateChip />
    </>
  )
}

function firstTargetId(s: Parameters<Parameters<typeof useStore>[0]>[0]): string | undefined {
  const c = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
  return (c.ready[0] ?? c.busy.find((b) => !b.install))?.botId
}

function rowText(s: Parameters<Parameters<typeof useStore>[0]>[0], botId: string, name: string, tail: string[], noIdentity: string): string {
  const bot = s.bots.find((b) => b.id === botId)
  const project = s.projects.find((p) => p.id === bot?.project_id)
  const host = projectHostName(s, bot?.project_id ?? null)
  return [name, project?.label, bot?.identity || noIdentity, host !== 'local' ? host : null, ...tail].filter((x): x is string => Boolean(x)).join(' · ')
}

export function RestartChip({ control }: { control?: DialogControl } = {}) {
  const t = useT()
  const batch = useStore((s) => s.restartBatch)
  const restartIdleBots = useStore((s) => s.restartIdleBots)
  const clear = useStore((s) => s.clearRestartBatch)
  const sending = useStore((s) => Boolean(s.busy['restart-idle']))
  const todo = useInstallTodo()
  const startChain = useStore((s) => s.startUpdateChain)
  const chain = useStore((s) => s.updateChain)
  const readyCount = useStore((s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.length)
  const readyNames = useStore(
    useShallow((s) =>
      updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.map((b) =>
        rowText(s, b.botId, b.name, b.backgroundUnknown ? [t('背景狀態未知')] : [], t('預設')),
      ),
    ),
  )
  const readyIds = useStore(useShallow((s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.map((b) => b.botId)))
  const unknownCount = useStore((s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.filter((b) => b.backgroundUnknown).length)
  const busyLines = useStore(
    useShallow((s) =>
      updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
        .busy.filter((b) => !b.install)
        .map((b) => rowText(s, b.botId, b.name, [b.why], t('預設'))),
    ),
  )
  const busyCount = busyLines.length
  const busyIds = useStore(
    useShallow((s) =>
      updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
        .busy.filter((b) => !b.install)
        .map((b) => b.botId),
    ),
  )
  const scheduled = useStore(useShallow((s) => s.updateRestartScheduled))
  const scheduleUpdateRestart = useStore((s) => s.scheduleUpdateRestart)
  const cancelUpdateRestart = useStore((s) => s.cancelUpdateRestart)
  const scheduledBusy = busyIds.filter((id) => scheduled.includes(id))
  const allScheduled = busyCount > 0 && scheduledBusy.length === busyCount
  const restartBot = useStore((s) => s.restartBot)
  const [force, setForce] = useState<{ ids: string[]; names: string[] } | null>(null)
  const busyOnly = readyCount === 0 && todo.count === 0
  const [localOpen, setLocalOpen] = useState(false)
  useUpdateChipRequest((k) => k === 'claude' || k === 'codex', () => { if (!control) setLocalOpen(true) })
  const confirming = control ? control.open : localOpen
  const setConfirming = (v: boolean) => {
    return control ? !v && control.close() : setLocalOpen(v)
  }
  const botHost = useStore((s) => {
    const first = firstTargetId(s)
    return projectHostName(s, s.bots.find((b) => b.id === first)?.project_id ?? null)
  })
  const botKind = useStore((s) => {
    const first = firstTargetId(s)
    return s.bots.find((b) => b.id === first)?.kind ?? 'claude'
  })
  const botFrom = useStore((s) => {
    const first = firstTargetId(s)
    return first ? (s.runs[first]?.status?.version ?? null) : null
  })
  const botNotice = useStore((s) => {
    const first = firstTargetId(s)
    return first ? (s.runs[first]?.update_notice ?? null) : null
  })
  const installHost = todo.claude?.toInstall[0] ?? todo.codex?.host
  const changelogKind = todo.claude && todo.claude.toInstall.length > 0 ? 'claude' : todo.codex ? 'codex' : botKind
  const changelogHost = installHost ?? botHost
  const claudeFrom = todo.claude?.hosts.find((h) => h.host === installHost)?.installedVersion ?? null
  const range =
    changelogKind === 'claude' && todo.claude && installHost
      ? { from: claudeFrom, to: todo.claude.target }
      : changelogKind === 'codex' && todo.codex
        ? updateRange('codex', todo.codex.notice, null)
        : updateRange(botKind, botNotice, botFrom)
  const canReview = changelogKind === 'claude' || changelogKind === 'codex'
  const review = useReviewAsk(changelogKind, changelogHost, range.from, range.to, false)

  if (todo.active.length > 0 && !control) {
    const cur = todo.active[0]
    const label = t('{host} 的 {kind} {phase}…（裝完會自動重啟閒置的 Bot，在忙的排成閒下來再重啟）', { host: cur.host, kind: cur.kind, phase: t.tr(CLI_UPDATE_PHASE_LABEL[cur.phase]) })
    return (
      <button type="button" className="quota-update install running" disabled title={label} aria-label={label}>
        <span aria-hidden="true"><UpgradeIcon /></span>
        <span className="quota-update-n" aria-hidden="true">…</span>
      </button>
    )
  }

  if (batch && !control) {
    const { total, done, ok, failed, finished } = batch
    const summary = finished
      ? failed.length
        ? t('重啟完成 · 成功 {ok} 顆 · 失敗 {failed} 顆', { ok: ok.length, failed: failed.length })
        : t('重啟完成 · 成功 {ok} 顆', { ok: ok.length })
      : t('重啟中 {done}/{total}', { done, total })
    return (
      <button
        type="button"
        className={`quota-update running${finished ? ' done' : ''}${failed.length ? ' bad' : ''}`}
        title={t('{summary}（點一下收起，詳細名單在側欄{tail}）', { summary, tail: finished ? '' : t('；不會中斷重啟') })}
        aria-label={summary}
        onClick={clear}
      >
        <span aria-hidden="true">{finished ? (failed.length ? '⚠' : '✓') : <UpgradeIcon />}</span>
        <span className="quota-update-n" aria-hidden="true">
          {finished ? ok.length : `${done}/${total}`}
        </span>
      </button>
    )
  }

  if (readyCount === 0 && busyCount === 0 && todo.count === 0 && !control) return null

  const installLines = [
    ...(todo.claude?.toInstall.map((host) => t('{host}：安裝 claude {target}', { host, target: todo.claude!.target })) ?? []),
    ...(todo.codex ? [t('{host}：安裝 codex{target}', { host: todo.codex.host, target: todo.codex.target ? ` ${todo.codex.target}` : '' })] : []),
  ]
  const total = todo.count + readyCount + busyCount

  const busyNote = busyCount > 0 ? t('\n{n} 顆在忙，會先跳過：\n{names}', { n: busyCount, names: busyLines.join('\n') }) : ''
  const installNote = installLines.length > 0 ? t('\n要安裝 {n} 台：\n{names}', { n: installLines.length, names: installLines.join('\n') }) : ''
  const label =
    todo.count > 0
      ? t('有 CLI 新版 · 點一下安裝並重啟（還有 {n} 項：安裝 {i}、重啟 {r}、在忙 {b}）', { n: total, i: todo.count, r: readyCount, b: busyCount })
      : readyCount === 0
      ? scheduledBusy.length > 0
        ? t('{n} 顆在忙，已排程閒下來自動重啟（點開可查看或取消）', { n: busyCount })
        : t('{n} 顆在忙，點開可安排閒下來自動重啟', { n: busyCount })
      : t('有 CLI 更新 · 重啟 {n} 顆閒置的 Bot（結束目前的 agent，再用同一個 session --resume 接回來，上下文不會掉）', { n: readyCount })

  return (
    <>
      {control ? null : <button
        type="button"
        className={`quota-update${todo.count > 0 ? ' install' : ''}${busyOnly ? ' waiting' : ''}${scheduledBusy.length > 0 ? ' scheduled' : ''}`}
        disabled={sending || chain}
        title={`${label}${installNote}${readyCount > 0 ? t('\n要重啟 {n} 顆：{names}', { n: readyCount, names: readyNames.join(t('、')) }) : ''}${busyNote}`}
        aria-label={label}
        onClick={() => setConfirming(true)}
      >
        <span aria-hidden="true"><UpgradeIcon /></span>
        <span className="quota-update-n" aria-hidden="true">
          {sending || chain ? '…' : total}
        </span>
        {scheduledBusy.length > 0 ? <span className="quota-update-clock" aria-hidden="true">⏱</span> : null}
      </button>}
      <ConfirmDialog
        open={confirming && force === null}
        title={todo.count > 0 ? t('安裝新版並重啟？') : busyOnly ? (allScheduled ? t('這些 Bot 已排成閒下來自動重啟') : t('這些 Bot 都在忙：閒下來自動重啟？')) : t('重啟這些 Bot 來套用更新？')}
        body={
          <>
            {confirming ? (
              <div className="update-split">
                <UpdateChangelog kind={changelogKind} host={changelogHost} from={range.from} to={range.to} />
                {}
                {canReview ? (
                  <AgmReviewBox kind={changelogKind} host={changelogHost} from={range.from} to={range.to} refreshKey={review.reviewKey} />
                ) : null}
              </div>
            ) : null}
            {todo.count > 0 ? (
              <>
                <p>{t('按下確認後依序：先在下列主機安裝新版，裝完自動重啟閒置的 Bot，在忙的排成閒下來自動重啟。任一台安裝失敗就不重啟任何一顆。')}</p>
                <ul className="confirm-list">
                  {installLines.map((n) => (
                    <li key={n}>{n}</li>
                  ))}
                </ul>
              </>
            ) : null}
            {busyOnly ? (
              <p>
                {t.x('有 CLI 更新，但以下 {n} 顆現在都在忙，不會打斷它們。排成「閒下來自動重啟」後，每顆下次閒置時會結束目前的 agent，再用同一個 session {resume} 接回來，上下文不會掉；可以隨時取消。', {
                  n: <strong>{busyCount}</strong>,
                  resume: <code>--resume</code>,
                })}
              </p>
            ) : null}
            {busyOnly ? null : <p>
              {t.x('以下 {n} 顆會結束目前的 agent，再用同一個 session {resume} 接回來——上下文不會掉，但重啟要花幾秒，這段時間它們不會回話。', {
                n: <strong>{readyCount}</strong>,
                resume: <code>--resume</code>,
              })}
            </p>}
            {busyOnly ? null : <ul className="confirm-list">
              {readyNames.map((n, i) => (
                <li key={`${i}:${n}`}>
                  <span className="confirm-row-text">{n}</span>
                  <button type="button" className="confirm-row-force" onClick={() => setForce({ ids: [readyIds[i]], names: [n] })}>
                    {t('立即重啟')}
                  </button>
                </li>
              ))}
            </ul>}
            {!busyOnly && unknownCount > 0 ? (
              <p className="confirm-note">
                {t('標「背景狀態未知」的 {n} 顆，daemon 還沒讀過它們畫面底部有沒有背景工作（剛重啟）；若有，重啟會一起結束。要保險就等 30 秒再按。', { n: unknownCount })}
              </p>
            ) : null}
            {busyCount > 0 ? (
              <>
                {busyOnly ? null : <p className="confirm-note">{t('另外 {n} 顆在忙，會排成閒下來自動重啟：', { n: busyCount })}</p>}
                <ul className="confirm-list dim">
                  {busyLines.map((n, i) => (
                    <li key={`${i}:${n}`}>
                      <span className="confirm-row-text">{scheduled.includes(busyIds[i]) ? `${n} · ${t('已排程')}` : n}</span>
                      <button type="button" className="confirm-row-force" onClick={() => setForce({ ids: [busyIds[i]], names: [n] })}>
                        {t('立即重啟')}
                      </button>
                    </li>
                  ))}
                </ul>
              </>
            ) : null}
            {readyCount + busyCount > 0 ? (
              <p>
                <button type="button" className="confirm-row-force all" onClick={() => setForce({ ids: [...readyIds, ...busyIds], names: [...readyNames, ...busyLines] })}>
                  {t('全部立即重啟')}
                </button>
              </p>
            ) : null}
          </>
        }
        confirmLabel={todo.count > 0 ? t('安裝並重啟') : busyOnly ? (allScheduled ? t('取消自動重啟') : t('閒下來自動重啟')) : t('重啟 {n} 顆', { n: readyCount })}
        secondaryLabel={canReview ? (review.asking ? t('派工中…') : t('分析影響')) : undefined}
        secondaryDisabled={review.asking}
        onSecondary={canReview ? review.ask : undefined}
        width={440}
        onCancel={() => setConfirming(false)}
        onConfirm={() => {
          const alsoIds = busyIds.filter((id) => !scheduled.includes(id))
          setConfirming(false)
          if (todo.count > 0) {
            void startChain({
              ...(todo.claude && todo.claude.toInstall.length > 0 ? { claude: { hosts: todo.claude.toInstall, target: todo.claude.target } } : {}),
              ...(todo.codex ? { codex: { host: todo.codex.host, target: todo.codex.target } } : {}),
            })
            return
          }
          if (busyOnly) {
            void (allScheduled ? cancelUpdateRestart(busyIds) : scheduleUpdateRestart(busyIds))
            return
          }
          void restartIdleBots().then(() => (alsoIds.length > 0 ? scheduleUpdateRestart(alsoIds) : undefined))
        }}
      />
      <ConfirmDialog
        open={force !== null}
        danger
        title={t('立即重啟？')}
        body={
          force ? (
            <>
              <p>
                {force.ids.length === 1
                  ? t.x('會中斷這顆目前的回合（Esc 停下），之後用 {resume} 接回，上下文不會掉，但正在進行的工作要重新下指令。', { resume: <code>--resume</code> })
                  : t.x('會中斷這 {n} 顆目前的回合（Esc 停下），之後用 {resume} 接回，上下文不會掉，但正在進行的工作要重新下指令。', { n: <strong>{force.ids.length}</strong>, resume: <code>--resume</code> })}
              </p>
              <ul className="confirm-list">
                {force.names.map((n, i) => (
                  <li key={`${i}:${n}`}>{n}</li>
                ))}
              </ul>
            </>
          ) : null
        }
        confirmLabel={t('立即重啟 {n} 顆', { n: force?.ids.length ?? 0 })}
        width={440}
        onCancel={() => setForce(null)}
        onConfirm={() => {
          const ids = force?.ids ?? []
          setForce(null)
          setConfirming(false)
          void (async () => {
            for (const id of ids) await restartBot(id)
          })()
        }}
      />
    </>
  )
}
