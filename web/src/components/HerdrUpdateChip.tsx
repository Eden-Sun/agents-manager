import { useState } from 'react'
import * as api from '../api'
import { useStore } from '../store/store'
import { useHerdrPlan } from '../hooks/useHerdrPlan'
import { reviewErrText } from '../lib/claudeReviewErr'
import {
  HERDR_PHASE_LABEL,
  dismissHerdrResult,
  herdrReasonText,
  startHerdr,
  useHerdrUpdate,
  type HerdrUpdateResult,
} from '../store/herdrUpdate'
import { ConfirmDialog } from './ConfirmDialog'
import { UpgradeIcon } from './UpgradeIcon'
import { UpdateChangelog } from './UpdateChangelog'
import { AgmReviewBox } from './AgmReviewBox'
import type { DialogControl } from './MergedUpdateChip'
import './herdrUpdate.css'

/**
 * header 的「herdr 有新版」（SPEC §6.9，使用者 2026-10-01：「跟 claude/codex 一樣 header 出現徽章 → 確認框 → 一鍵更新」）。
 * 跟 codex／claude 那兩顆同一個樣子、同一種確認框（左 changelog、右 AGM 解析），但後果大得多：herdr server 整個重啟，
 * **所有 Bot 都會中斷**，子 agent 一律沒了。所以框裡最顯眼的是那句警告，下面列會接回的與會失去的。
 * 數字位置寫 `herdr` 而不是顆數：三顆並排時要一眼分得出這顆是哪一種。
 */
export function HerdrUpdateChip({ control }: { control?: DialogControl } = {}) {
  const plan = useHerdrPlan()
  const active = useHerdrUpdate((s) => s.active)
  const result = useHerdrUpdate((s) => s.result)
  const notify = useStore((s) => s.notify)
  const [localOpen, setLocalOpen] = useState(false)
  const open = control ? control.open : localOpen
  const close = () => (control ? control.close() : setLocalOpen(false))
  const [asking, setAsking] = useState(false)
  const [reviewKey, setReviewKey] = useState(0)

  if (active) {
    if (control) return null
    const label = `herdr 更新中：${HERDR_PHASE_LABEL[active.phase]}（${active.host}${active.target ? ` → ${active.target}` : ''}）${
      active.phase === 'waiting_idle' ? '。等所有 Bot 都閒下來才重啟，最多 30 分鐘' : ''
    }${active.willResume.length ? `\n重啟後接回：${active.willResume.map((b) => b.name).join('、')}` : ''}`
    return (
      <button type="button" className="quota-update install running" disabled title={label} aria-label={label}>
        <span aria-hidden="true"><UpgradeIcon /></span>
        <span className="quota-update-n herdr-chip-tag" aria-hidden="true">herdr…</span>
      </button>
    )
  }

  if (result) return <ResultChip result={result} control={control} />
  if (!plan) return null

  const target = plan.target
  const from = plan.from
  const host = plan.host
  const label = host
    ? `herdr 有新版 ${target}（${host}：${from ?? '讀取失敗'}）· 點一下更新：herdr 會整個重啟，所有 Bot 中斷約 1 分鐘後接回`
    : `herdr 有新版 ${target}，但不能從這裡更新：${plan.blockedWhy ?? ''}`

  return (
    <>
      {control ? null : (
        <button
          type="button"
          className={`quota-update${host ? ' install' : ' waiting'}`}
          title={label}
          aria-label={label}
          onClick={() => setLocalOpen(true)}
        >
          <span aria-hidden="true"><UpgradeIcon /></span>
          <span className="quota-update-n herdr-chip-tag" aria-hidden="true">herdr</span>
        </button>
      )}
      <ConfirmDialog
        open={open}
        title={`更新 herdr 到 ${target} 並重啟？`}
        danger
        body={
          <>
            {open ? (
              <div className="update-split">
                <UpdateChangelog kind="herdr" host={host ?? 'local'} from={from} to={target} />
                <AgmReviewBox kind="herdr" host={host ?? 'local'} from={from} to={target} refreshKey={reviewKey} />
              </div>
            ) : null}
            <p className="herdr-update-warn" role="alert">
              <strong>所有 Bot 會中斷約 1 分鐘</strong>：herdr server 要整個重啟，每顆 Bot 的終端都會斷掉，再用同一個 session 接回來。
            </p>
            {host ? (
              <>
                <p>
                  先在 <strong>{host}</strong> 下載 {target} 並驗證版本（這時還不換），再等所有頂層 Bot 都閒下來（最多 30 分鐘，等不到就什麼都不動），
                  然後換上新版、重啟 herdr。新版起不來會自動換回 {from ?? '舊版'}。
                </p>
                {plan.willResume.length > 0 ? (
                  <>
                    <p className="confirm-note herdr-update-sub">重啟後接回這 {plan.willResume.length} 顆：</p>
                    <ul className="confirm-list">
                      {plan.willResume.map((b) => (
                        <li key={b.botId}>{b.name}</li>
                      ))}
                    </ul>
                  </>
                ) : (
                  <p className="confirm-note">{host} 現在沒有在跑的 Bot，重啟後沒有要接回的。</p>
                )}
                {plan.childrenLost.length > 0 ? (
                  <>
                    <p className="confirm-note herdr-update-lost">
                      這 {plan.childrenLost.length} 個子 agent 會被結束、<strong>不會自動接回</strong>；更新完會通知它們的母 Bot，由母 Bot 視需要重開：
                    </p>
                    <ul className="confirm-list dim">
                      {plan.childrenLost.map((b) => (
                        <li key={b.botId}>
                          {b.name}（母：{b.parentName}）
                        </li>
                      ))}
                    </ul>
                  </>
                ) : null}
              </>
            ) : (
              <p className="confirm-note herdr-update-lost">{plan.blockedWhy}</p>
            )}
            {plan.hosts.length > 1 || !host ? (
              <ul className="confirm-list dim herdr-update-hosts">
                {plan.hosts.map((h) => (
                  <li key={h.host}>
                    {h.host}：{h.installed ?? `讀取失敗${h.error ? `（${h.error}）` : ''}`}
                    {h.behind ? ` → ${target}` : '（已是最新）'}
                    {h.behind && h.blocked ? ` · ${h.blocked.split('：')[0]}` : ''}
                  </li>
                ))}
              </ul>
            ) : null}
          </>
        }
        confirmLabel={host ? `更新並重啟 herdr` : '無法從這裡更新'}
        confirmDisabled={!host}
        secondaryLabel={asking ? '派工中…' : '請 AGM 解析'}
        secondaryDisabled={asking}
        onSecondary={() => {
          setAsking(true)
          void api
            .requestUpdateReview({ kind: 'herdr', host: host ?? 'local', from, to: target })
            .then((r) => {
              setReviewKey((n) => n + 1)
              notify(
                'info',
                r.duplicate
                  ? `herdr ${r.version} 已經派給 ${r.target_bot_name || 'AGM'} 解析過了，結論會回到這裡`
                  : `已請 ${r.target_bot_name} 解析 herdr ${r.version} 的 changelog，結論會回到這裡`,
              )
            })
            .catch((e: unknown) => notify('error', `派不出去：${reviewErrText(e)}`))
            .finally(() => setAsking(false))
        }}
        width={480}
        onCancel={close}
        onConfirm={() => {
          close()
          if (host) void startHerdr(host, target, notify)
        }}
      />
    </>
  )
}

/** 更新跑完：✓／⚠ 留在 header，點開看誰接回、誰沒接回、哪些子 agent 沒了；「知道了」才收起。 */
function ResultChip({ result, control }: { result: HerdrUpdateResult; control?: DialogControl }) {
  const bots = useStore((s) => s.bots)
  const [localOpen, setLocalOpen] = useState(false)
  const open = control ? control.open : localOpen
  const close = () => (control ? control.close() : setLocalOpen(false))
  const bad = !result.ok || result.failed.length > 0
  const nameOf = (id: string) => bots.find((b) => b.id === id)?.name ?? '（已不在）'
  const summary = result.ok
    ? `herdr 已升到 ${result.to ?? '新版'} · 接回 ${result.resumed.length} 顆${result.failed.length ? ` · ${result.failed.length} 顆沒接回` : ''}`
    : `herdr 沒有升級（${herdrReasonText(result.reason)}）`
  return (
    <>
      {control ? null : (
        <button
          type="button"
          className={`quota-update running done${bad ? ' bad' : ''}`}
          title={`${summary}（點一下看名單）`}
          aria-label={summary}
          onClick={() => setLocalOpen(true)}
        >
          <span aria-hidden="true">{bad ? '⚠' : '✓'}</span>
          <span className="quota-update-n herdr-chip-tag" aria-hidden="true">herdr</span>
        </button>
      )}
      <ConfirmDialog
        open={open}
        title={result.ok ? `herdr 已升到 ${result.to ?? '新版'}` : 'herdr 沒有升級'}
        body={
          <>
            <p>
              {result.ok
                ? `${result.host}：${result.from ?? '舊版'} → ${result.to ?? '新版'}。`
                : `${result.host}：${herdrReasonText(result.reason)}${result.detail ? `（${result.detail}）` : ''}。`}
            </p>
            {result.resumed.length > 0 ? (
              <>
                <p className="confirm-note herdr-update-sub">已接回 {result.resumed.length} 顆：</p>
                <ul className="confirm-list">
                  {result.resumed.map((b) => (
                    <li key={b.bot_id}>{b.name}</li>
                  ))}
                </ul>
              </>
            ) : null}
            {result.failed.length > 0 ? (
              <>
                <p className="confirm-note herdr-update-lost">沒接回 {result.failed.length} 顆（到那顆按「啟動」再試）：</p>
                <ul className="confirm-list">
                  {result.failed.map((b) => (
                    <li key={b.bot_id}>
                      {b.name}
                      {b.error ? `（${b.error}）` : ''}
                    </li>
                  ))}
                </ul>
              </>
            ) : null}
            {result.childrenLost.length > 0 ? (
              <>
                <p className="confirm-note">已結束的子 agent（已通知母 Bot 視需要重開）：</p>
                <ul className="confirm-list dim">
                  {result.childrenLost.map((b) => (
                    <li key={b.bot_id}>
                      {b.name}（母：{nameOf(b.parent_bot_id)}）
                    </li>
                  ))}
                </ul>
              </>
            ) : null}
          </>
        }
        confirmLabel="知道了"
        cancelLabel="先留著"
        width={440}
        onCancel={close}
        onConfirm={() => {
          close()
          dismissHerdrResult()
        }}
      />
    </>
  )
}
