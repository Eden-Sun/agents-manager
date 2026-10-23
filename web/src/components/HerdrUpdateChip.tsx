import { useState } from 'react'
import * as api from '../api'
import { useStore } from '../store/store'
import { useHerdrPlan } from '../hooks/useHerdrPlan'
import { reviewErrText } from '../lib/claudeReviewErr'
import { useT } from '../lib/i18n'
import {
  HERDR_PHASE_LABEL,
  dismissHerdrResult,
  herdrBinaryText,
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
  const t = useT()
  const plan = useHerdrPlan()
  const active = useHerdrUpdate((s) => s.active)
  const result = useHerdrUpdate((s) => s.result)
  const notify = useStore((s) => s.notify)
  const herdrVersions = useStore((s) => s.hosts.find((h) => h.name === (plan?.host ?? ''))?.herdr)
  const [localOpen, setLocalOpen] = useState(false)
  const open = control ? control.open : localOpen
  const close = () => (control ? control.close() : setLocalOpen(false))
  const [asking, setAsking] = useState(false)
  const [reviewKey, setReviewKey] = useState(0)

  if (active) {
    if (control) return null
    const label = t('herdr 更新中：{phase}（{where}）{wait}{resume}', {
      phase: t.tr(HERDR_PHASE_LABEL[active.phase]),
      where: `${active.host}${active.target ? ` → ${active.target}` : ''}`,
      wait: active.phase === 'waiting_idle' ? t('。等所有 Bot 都閒下來才重啟，最多 30 分鐘') : '',
      resume: active.willResume.length ? t('\n重啟後接回：{names}', { names: active.willResume.map((b) => b.name).join(t('、')) }) : '',
    })
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
    ? t('herdr 有新版 {target}（{host}：{from}）· 點一下更新：herdr 會整個重啟，所有 Bot 中斷約 1 分鐘後接回', {
        target,
        host,
        from: from ?? t('讀取失敗'),
      })
    : t('herdr 有新版 {target}，但不能從這裡更新：{why}', { target, why: plan.blockedWhy ?? '' })

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
        title={t('更新 herdr 到 {target} 並重啟？', { target })}
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
              {t.x('{bold}：herdr server 要整個重啟，每顆 Bot 的終端都會斷掉，再用同一個 session 接回來。', { bold: <strong>{t('所有 Bot 會中斷約 1 分鐘')}</strong> })}
            </p>
            {host ? (
              <>
                <p>
                  {t.x(
                    '先在 {host} 下載 {target} 並驗證版本（這時還不換），再等所有頂層 Bot 都閒下來（最多 30 分鐘，等不到就什麼都不動），然後換上新版、重啟 herdr。新版起不來會自動換回 {from}。',
                    { host: <strong>{host}</strong>, target, from: from ?? t('舊版') },
                  )}
                </p>
                {herdrVersions?.mismatch ? (
                  <p className="confirm-note herdr-update-sub">
                    {t.x(
                      '目前 server 是 {server}、CLI 是 {cli}：{older}那一份比較舊；同一台有兩份 herdr（launchd／systemd 跑的與 shim 用的不同檔）時，兩份會一起換。',
                      {
                        server: herdrVersions.server_version ?? t('讀不到'),
                        cli: herdrVersions.cli_version ?? t('讀不到'),
                        older: herdrOlder(herdrVersions.server_version, herdrVersions.cli_version),
                      },
                    )}
                  </p>
                ) : null}
                {plan.willResume.length > 0 ? (
                  <>
                    <p className="confirm-note herdr-update-sub">{t('重啟後接回這 {n} 顆：', { n: plan.willResume.length })}</p>
                    <ul className="confirm-list">
                      {plan.willResume.map((b) => (
                        <li key={b.botId}>{b.name}</li>
                      ))}
                    </ul>
                  </>
                ) : (
                  <p className="confirm-note">{t('{host} 現在沒有在跑的 Bot，重啟後沒有要接回的。', { host })}</p>
                )}
                {plan.childrenLost.length > 0 ? (
                  <>
                    <p className="confirm-note herdr-update-lost">
                      {t('這 {n} 個子 agent 會被結束、不會自動接回；更新完會通知它們的母 Bot，由母 Bot 視需要重開：', { n: plan.childrenLost.length })}
                    </p>
                    <ul className="confirm-list dim">
                      {plan.childrenLost.map((b) => (
                        <li key={b.botId}>
                          {t('{name}（母：{parent}）', { name: b.name, parent: b.parentName })}
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
                    {t('{host}：{state}{arrow}{blocked}', {
                      host: h.host,
                      state: h.installed ?? (h.error ? t('讀取失敗（{error}）', { error: h.error }) : t('讀取失敗')),
                      arrow: h.behind ? ` → ${target}` : t('（已是最新）'),
                      blocked: h.behind && h.blocked ? ` · ${h.blocked.split('：')[0]}` : '', // i18n-ignore：拆 daemon 的說明句，中文分隔符是資料格式
                    })}
                  </li>
                ))}
              </ul>
            ) : null}
          </>
        }
        confirmLabel={host ? t('更新並重啟 herdr') : t('無法從這裡更新')}
        confirmDisabled={!host}
        secondaryLabel={asking ? t('派工中…') : t('請 AGM 解析')}
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
                  ? t('herdr {version} 已經派給 {bot} 解析過了，結論會回到這裡', { version: r.version, bot: r.target_bot_name || 'AGM' })
                  : t('已請 {bot} 解析 herdr {version} 的 changelog，結論會回到這裡', { version: r.version, bot: r.target_bot_name }),
              )
            })
            .catch((e: unknown) => notify('error', t('派不出去：{reason}', { reason: reviewErrText(e) })))
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

/** server／CLI 版本不同時，較舊的是哪一份。 */
function herdrOlder(server: string | null, cli: string | null): string {
  const v = (x: string | null) => (x ?? '').split('.').map((n) => Number.parseInt(n, 10) || 0)
  const [a, b] = [v(server), v(cli)]
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) < (b[i] ?? 0) ? 'server ' : 'CLI '
  }
  return ''
}

/** 更新跑完：✓／⚠ 留在 header，點開看誰接回、誰沒接回、哪些子 agent 沒了；「知道了」才收起。 */
function ResultChip({ result, control }: { result: HerdrUpdateResult; control?: DialogControl }) {
  const t = useT()
  const bots = useStore((s) => s.bots)
  const [localOpen, setLocalOpen] = useState(false)
  const open = control ? control.open : localOpen
  const close = () => (control ? control.close() : setLocalOpen(false))
  const bad = !result.ok || result.failed.length > 0
  const nameOf = (id: string) => bots.find((b) => b.id === id)?.name ?? t('（已不在）')
  const to = result.to ?? t('新版')
  const summary = result.ok
    ? result.failed.length
      ? t('herdr 已升到 {to} · 接回 {n} 顆 · {failed} 顆沒接回', { to, n: result.resumed.length, failed: result.failed.length })
      : t('herdr 已升到 {to} · 接回 {n} 顆', { to, n: result.resumed.length })
    : t('herdr 沒有升級（{reason}）', { reason: herdrReasonText(result.reason) })
  return (
    <>
      {control ? null : (
        <button
          type="button"
          className={`quota-update running done${bad ? ' bad' : ''}`}
          title={t('{summary}（點一下看名單）', { summary })}
          aria-label={summary}
          onClick={() => setLocalOpen(true)}
        >
          <span aria-hidden="true">{bad ? '⚠' : '✓'}</span>
          <span className="quota-update-n herdr-chip-tag" aria-hidden="true">herdr</span>
        </button>
      )}
      <ConfirmDialog
        open={open}
        title={result.ok ? t('herdr 已升到 {to}', { to }) : t('herdr 沒有升級')}
        body={
          <>
            <p>
              {result.ok
                ? t('{host}：{from} → {to}。', { host: result.host, from: result.from ?? t('舊版'), to })
                : t('{host}：{reason}{detail}。', {
                    host: result.host,
                    reason: herdrReasonText(result.reason),
                    detail: result.detail ? t('（{detail}）', { detail: result.detail }) : '',
                  })}
            </p>
            {result.binaries.length > 1 || (result.binaries.length === 1 && !result.ok) ? (
              <>
                <p className="confirm-note herdr-update-sub">{t('這台的 herdr：')}</p>
                <ul className="confirm-list dim">
                  {result.binaries.map((b) => (
                    <li key={b.path}>{herdrBinaryText(b)}</li>
                  ))}
                </ul>
              </>
            ) : null}
            {!result.ok && result.manualCommand && (result.reason === 'install_path_unsupported' || result.reason === 'restart_unsupported') ? (
              <>
                <p className="confirm-note herdr-update-sub">
                  {t('在 {host} 上手動升級（只換 binary；升完再回來按更新，或自己重啟 herdr server）：', { host: result.host })}
                </p>
                <pre className="herdr-update-cmd">{result.manualCommand}</pre>
              </>
            ) : null}
            {result.resumed.length > 0 ? (
              <>
                <p className="confirm-note herdr-update-sub">{t('已接回 {n} 顆：', { n: result.resumed.length })}</p>
                <ul className="confirm-list">
                  {result.resumed.map((b) => (
                    <li key={b.bot_id}>{b.name}</li>
                  ))}
                </ul>
              </>
            ) : null}
            {result.failed.length > 0 ? (
              <>
                <p className="confirm-note herdr-update-lost">{t('沒接回 {n} 顆（到那顆按「啟動」再試）：', { n: result.failed.length })}</p>
                <ul className="confirm-list">
                  {result.failed.map((b) => (
                    <li key={b.bot_id}>
                      {b.name}
                      {b.error ? t('（{error}）', { error: b.error }) : ''}
                    </li>
                  ))}
                </ul>
              </>
            ) : null}
            {result.childrenLost.length > 0 ? (
              <>
                <p className="confirm-note">{t('已結束的子 agent（已通知母 Bot 視需要重開）：')}</p>
                <ul className="confirm-list dim">
                  {result.childrenLost.map((b) => (
                    <li key={b.bot_id}>
                      {t('{name}（母：{parent}）', { name: b.name, parent: nameOf(b.parent_bot_id) })}
                    </li>
                  ))}
                </ul>
              </>
            ) : null}
          </>
        }
        confirmLabel={t('知道了')}
        cancelLabel={t('先留著')}
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
