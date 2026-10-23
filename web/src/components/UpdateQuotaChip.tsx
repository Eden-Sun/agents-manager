import { useState } from 'react'
import { useShallow } from 'zustand/react/shallow'
import * as api from '../api'
import { inFlightTurn, projectHostName, useStore } from '../store/store'
import { readyLabel, updateBatchCounts } from '../lib/updateBatch'
import { reviewErrText } from '../lib/claudeReviewErr'
import { updateRange } from '../lib/updateRange'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'
import { AgmReviewBox } from './AgmReviewBox'
import { CodexInstallChip } from './CodexInstallChip'
import { ClaudeInstallChip } from './ClaudeInstallChip'
import { HerdrUpdateChip } from './HerdrUpdateChip'
import { GrokUpdateChip } from './GrokUpdateChip'
import { AgyUpdateChip } from './AgyUpdateChip'
import { useHerdrPlan } from '../hooks/useHerdrPlan'
import { useGrokPlan } from '../hooks/useGrokPlan'
import { useAgyPlan } from '../hooks/useAgyPlan'
import { useHerdrUpdate } from '../store/herdrUpdate'
import { MergedUpdateChip, type DialogControl } from './MergedUpdateChip'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { claudeInstallPlan, codexInstallPlan, mergeUpdateChips } from '../lib/updateBatch'
import './updateQuotaChip.css'

/**
 * 「claude 有更新」擺在額度列最左邊（SPEC §6.9；位置與外觀 2026-09-11 使用者定）：更新與額度都是 kind 的全域狀態，
 * 側欄收起時也要看得到。對齊量表第一行並補 1px 分隔線（2026-09-11）。按下去先確認：批次重啟沒有取消，確認框是唯一反悔點。
 */
export function UpdateQuotaChip() {
  const phone = useMediaQuery(PHONE_QUERY)
  const restartShown = useStore((s) => {
    if (s.restartBatch) return true
    const c = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
    return c.ready.length > 0 || c.busy.some((b) => !b.install)
  })
  const codexShown = useStore(
    (s) =>
      s.cliUpdates.some((item) => item.kind === 'codex') ||
      codexInstallPlan(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null, (b) => projectHostName(s, b.project_id), s.upstreamUpdates.codex) !== null,
  )
  const claudeShown = useStore((s) =>
    Boolean(claudeInstallPlan(s.upstreamUpdates.claude)) || s.cliUpdates.some((item) => item.kind === 'claude'),
  )
  const herdrPlan = useHerdrPlan() !== null
  const herdrShown = useHerdrUpdate((s) => s.active !== null || s.result !== null) || herdrPlan
  const grokShown = useGrokPlan() !== null
  const agyShown = useAgyPlan() !== null
  // 手機兩種以上更新 chip 都要出現時合成一顆，選單分開列重啟、兩種 CLI 安裝與 herdr。
  if (mergeUpdateChips(phone, restartShown, codexShown, claudeShown, herdrShown, grokShown, agyShown)) return <MergedUpdateChip />
  return (
    <>
      <RestartChip />
      <CodexInstallChip />
      <ClaudeInstallChip />
      <HerdrUpdateChip />
      {/* grok 只提示、不一鍵安裝；手機單獨出現照舊一顆，跟別種並存時併進上面的合成選單。 */}
      <GrokUpdateChip />
      {/* agy 有一鍵安裝（issue #1921）；同樣的手機合併規則。 */}
      <AgyUpdateChip />
    </>
  )
}

/** `control`：手機合成的那顆（`MergedUpdateChip`）代為開框，這裡只畫確認框、不畫按鈕。 */
export function RestartChip({ control }: { control?: DialogControl } = {}) {
  const t = useT()
  const batch = useStore((s) => s.restartBatch)
  const restartIdleBots = useStore((s) => s.restartIdleBots)
  const clear = useStore((s) => s.clearRestartBatch)
  const sending = useStore((s) => Boolean(s.busy['restart-idle']))
  // 各 selector 回純值或字串陣列：`updateBatchCounts` 每次回新陣列，包成物件連 `useShallow` 都擋不住（getSnapshot should be cached）；
  // 字串陣列用 `useShallow` 逐項比就穩。名單不用「、」「\n」串成一個字串再拆（名字可以含「、」，#998）。
  const readyCount = useStore((s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.length)
  const readyNames = useStore(
    useShallow((s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.map(readyLabel)),
  )
  // 巡邏還沒看過它們的背景工作（#767）：沒有證據所以照樣會重啟，確認框要講清楚。
  const unknownCount = useStore((s) => updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready.filter((b) => b.backgroundUnknown).length)
  // 還沒裝新版的 codex 由旁邊的 `CodexInstallChip` 負責（安裝＋重啟），不再算進這顆的「在忙」。
  const busyLines = useStore(
    useShallow((s) =>
      updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null)
        .busy.filter((b) => !b.install)
        .map((b) => t('{name}（{why}）', { name: b.name, why: b.why })),
    ),
  )
  const busyCount = busyLines.length
  const [localOpen, setLocalOpen] = useState(false)
  const confirming = control ? control.open : localOpen
  const setConfirming = (v: boolean) => (control ? !v && control.close() : setLocalOpen(v))
  const [asking, setAsking] = useState(false)
  const [reviewKey, setReviewKey] = useState(0)
  const notify = useStore((s) => s.notify)
  // changelog 用第一顆等著套用的 bot 所在主機、kind 與它跑著的版本；同一台同 kind 都是同一份。
  const changelogHost = useStore((s) => {
    const first = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready[0]?.botId
    return projectHostName(s, s.bots.find((b) => b.id === first)?.project_id ?? null)
  })
  const changelogKind = useStore((s) => {
    const first = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready[0]?.botId
    return s.bots.find((b) => b.id === first)?.kind ?? 'claude'
  })
  const changelogFrom = useStore((s) => {
    const first = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready[0]?.botId
    return first ? (s.runs[first]?.status?.version ?? null) : null
  })
  const changelogNotice = useStore((s) => {
    const first = updateBatchCounts(s.bots, s.runs, (id) => inFlightTurn(s, id) !== null).ready[0]?.botId
    return first ? (s.runs[first]?.update_notice ?? null) : null
  })
  // codex 的版本區間從通知讀（磁碟已裝好的那種通知也寫著兩個版本），claude 照舊（issue #561）。
  const range = updateRange(changelogKind, changelogNotice, changelogFrom)
  const canReview = changelogKind === 'claude' || changelogKind === 'codex'

  if (batch && !control) {
    const { total, done, ok, failed, finished } = batch
    const summary = finished
      ? failed.length
        ? t('重啟完成 · 成功 {ok} 顆 · 失敗 {failed} 顆', { ok: ok.length, failed: failed.length })
        : t('重啟完成 · 成功 {ok} 顆', { ok: ok.length })
      : t('重啟中 {done}/{total}', { done, total })
    return (
      // 未完成也可以收起（issue #492）：以前 `disabled={!finished}`，`bots_restart_done` 收不到時
      // 這顆會永遠停用，而它又蓋住一鍵重啟的觸發鈕，等於連再按一次都不行。收起不中斷批次。
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

  if (readyCount === 0 && busyCount === 0 && !control) return null

  const busyNote = busyCount > 0 ? t('\n{n} 顆在忙，會先跳過：\n{names}', { n: busyCount, names: busyLines.join('\n') }) : ''
  const label =
    readyCount === 0
      ? t('有 CLI 更新，但這些 Bot 現在都在忙——閒下來再按')
      : t('有 CLI 更新 · 重啟 {n} 顆閒置的 Bot（結束目前的 agent，再用同一個 session --resume 接回來，上下文不會掉）', { n: readyCount })

  return (
    <>
      {control ? null : <button
        type="button"
        className={`quota-update${readyCount === 0 ? ' waiting' : ''}`}
        disabled={sending || readyCount === 0}
        title={`${label}${readyCount > 0 ? t('：\n{names}', { names: readyNames.join(t('、')) }) : ''}${busyNote}`}
        aria-label={label}
        onClick={() => setConfirming(true)}
      >
        <span aria-hidden="true"><UpgradeIcon /></span>
        <span className="quota-update-n" aria-hidden="true">
          {sending ? '…' : readyCount === 0 ? busyCount : readyCount}
        </span>
      </button>}
      <ConfirmDialog
        open={confirming}
        title={t('重啟這些 Bot 來套用更新？')}
        body={
          <>
            {confirming ? (
              <div className="update-split">
                <UpdateChangelog kind={changelogKind} host={changelogHost} from={range.from} to={range.to} />
                {/* 分析（上游分診＋AGM 解析）claude 與 codex 同一套（issue #561）。 */}
                {canReview ? (
                  <AgmReviewBox kind={changelogKind} host={changelogHost} from={range.from} to={range.to} refreshKey={reviewKey} />
                ) : null}
              </div>
            ) : null}
            <p>
              {t.x('以下 {n} 顆會結束目前的 agent，再用同一個 session {resume} 接回來——上下文不會掉，但重啟要花幾秒，這段時間它們不會回話。', {
                n: <strong>{readyCount}</strong>,
                resume: <code>--resume</code>,
              })}
            </p>
            <ul className="confirm-list">
              {readyNames.map((n, i) => (
                <li key={`${i}:${n}`}>{n}</li>
              ))}
            </ul>
            {unknownCount > 0 ? (
              <p className="confirm-note">
                {t('標「背景狀態未知」的 {n} 顆，daemon 還沒讀過它們畫面底部有沒有背景工作（剛重啟）；若有，重啟會一起結束。要保險就等 30 秒再按。', { n: unknownCount })}
              </p>
            ) : null}
            {busyCount > 0 ? (
              <>
                <p className="confirm-note">{t('另外 {n} 顆在忙，這次會跳過：', { n: busyCount })}</p>
                <ul className="confirm-list dim">
                  {busyLines.map((n, i) => (
                    <li key={`${i}:${n}`}>{n}</li>
                  ))}
                </ul>
              </>
            ) : null}
          </>
        }
        confirmLabel={t('重啟 {n} 顆', { n: readyCount })}
        // 批次框跟單顆框同一顆按鈕：更新的解析是**版本層級**的事，跟要重啟幾顆無關。
        // 單顆的 chip 在批次蓋得到時會自己隱藏，所以只做在 UpdateBadge 上等於多數情況看不到
        // （使用者 2026-09-19：「還是沒見按鈕」）。claude 與 codex 同一套（issue #561）。
        secondaryLabel={canReview ? (asking ? t('派工中…') : t('請 AGM 解析')) : undefined}
        secondaryDisabled={asking}
        onSecondary={canReview ? () => {
          setAsking(true)
          void api
            .requestUpdateReview({ kind: changelogKind, host: changelogHost, from: range.from, to: range.to })
            .then((r) => {
              // 框留著：結論直接顯示在裡面。
              setReviewKey((n) => n + 1)
              notify(
                'info',
                r.duplicate
                  ? t('{kind} {version} 已經派給 {bot} 解析過了，結論會回到這裡', { kind: changelogKind, version: r.version, bot: r.target_bot_name || 'AGM' })
                  : t('已請 {bot} 解析 {kind} {version} 的 changelog，結論會回到這裡', { bot: r.target_bot_name, kind: changelogKind, version: r.version }),
              )
            })
            .catch((e: unknown) => notify('error', t('派不出去：{reason}', { reason: reviewErrText(e) })))
            .finally(() => setAsking(false))
        } : undefined}
        width={440}
        onCancel={() => setConfirming(false)}
        onConfirm={() => {
          setConfirming(false)
          void restartIdleBots()
        }}
      />
    </>
  )
}
