import { useState } from 'react'
import { balanceView, putClaudeApiBalanceAnchor } from '../api/claudeApiBalance'
import { reloadClaudeApiBalance, useClaudeApiBalance } from '../hooks/useClaudeApiBalance'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { anchorAge, balanceTooltipLines } from './balanceTooltip'
import { Modal } from './Modal'
import './balanceQuotaChip.css'

const hhmm = (iso: string | null | undefined): string => {
  const d = iso ? new Date(iso) : null
  if (!d || Number.isNaN(d.getTime())) return '--:--'
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

/**
 * 全域額度列的 Anthropic API 餘額（使用者 2026-11-15：「額度列找個位置放餘額」）。
 * 數字：Console 讀得到就是 Console 實際值（`API $23.29`），否則是用同一套公式的**估算**（`API ≈$23.29`：錨點－官方報表－本機已確定花費，
 * 預留與結果不明的不扣、列在 tooltip）。手機縮成短格式 `$23.3`。沒設定（沒有錨點也讀不到 Console）就整格不畫。
 * 點下去開小視窗：列出明細、距上次校正多久，可輸入 Console 目前餘額校正錨點（官方沒有餘額端點，儲值／贈送額度只能靠校正）；有 claude-api bot 時另有「到 claude-api bot」。
 */
export function BalanceQuotaChip() {
  const t = useT()
  const phone = useMediaQuery(PHONE_QUERY)
  const balance = useClaudeApiBalance()
  const apiBotId = useStore((s) => s.bots.find((b) => b.kind === 'claude-api')?.id ?? null)
  const selectBot = useStore((s) => s.selectBot)
  const notify = useStore((s) => s.notify)
  const [open, setOpen] = useState(false)
  const [usdText, setUsdText] = useState('')
  const [saving, setSaving] = useState(false)
  const view = balanceView(balance)
  if (view.usd === undefined) return null
  const exact = view.source === 'console'
  const usd = view.usd
  // 估算成負數＝錨點已經過舊（期間有儲值或贈送額度：官方查得到花費、查不到儲值）：顯示「需校正」，不要把一個必錯的負數當餘額。
  const stale = !exact && usd < 0
  const text = stale ? t('需校正') : phone ? `$${usd.toFixed(1)}` : `${exact ? '' : '≈'}$${usd.toFixed(2)}`
  const lines = balanceTooltipLines(balance, view, t)
  const stamp = exact ? balance?.console?.fetched_at : balance?.as_of
  const title = [
    stale
      ? t('餘額估算成負數：錨點已過舊（之後有儲值或贈送額度，官方查不到）。點一下用 Console 目前餘額校正')
      : exact
        ? t('Anthropic API 餘額（Console 實際值）')
        : t('Anthropic API 餘額（估算）'),
    ...lines,
    t('更新於 {time}', { time: hhmm(stamp) }),
    t('點一下：用 Console 目前餘額校正，或到 claude-api bot'),
  ]
    .filter(Boolean)
    .join('\n')
  const warn = stale || view.loggedOut || view.expiringSoon || (balance?.uncertain_usd ?? 0) > 0
  const cls = `quota-balance${exact ? ' exact' : ' estimate'}${warn ? ' warn' : ''}`
  const body = (
    <>
      {phone ? null : <span className="quota-balance-k">API</span>}
      <span className="quota-balance-v">{text}</span>
    </>
  )
  const usdVal = Number.parseFloat(usdText)
  const save = async () => {
    if (!Number.isFinite(usdVal) || usdVal < 0 || saving) return
    setSaving(true)
    try {
      await putClaudeApiBalanceAnchor(usdVal)
      await reloadClaudeApiBalance()
      notify('info', t('已用 Console 餘額 ${usd} 校正', { usd: usdVal.toFixed(2) }))
      setOpen(false)
      setUsdText('')
    } catch (err) {
      notify('error', err instanceof Error ? err.message : String(err))
    } finally {
      setSaving(false)
    }
  }
  return (
    <>
      <button type="button" className={cls} data-k="quota-balance" title={title} onClick={() => setOpen(true)}>
        {body}
      </button>
      <Modal open={open} title={t('Anthropic API 餘額')} width={420} onClose={() => setOpen(false)}>
        <div className="quota-balance-dialog" data-k="quota-balance-dialog">
          {stale ? (
            <p className="hint warn" data-k="stale-note">
              {t('估算出來是負的（{usd}）：從錨點以來官方記到的花費已經超過錨點金額，表示期間有儲值或贈送額度。請用 Console 目前餘額校正。', { usd: `$${usd.toFixed(2)}` })}
            </p>
          ) : null}
          {lines.map((l) => (
            <p key={l} className="hint">{l}</p>
          ))}
          <label className="field">
            <span>{t('以 Console 目前餘額校正（美元）')}</span>
            <input
              type="number"
              step="0.01"
              min="0"
              autoFocus
              value={usdText}
              placeholder={t('例如 65.67')}
              onChange={(e) => setUsdText(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter') void save() }}
            />
          </label>
          <p className="hint" data-k="anchor-age">
            {balance?.anchor ? t('距上次校正：{age}', { age: anchorAge(balance.anchor.at, new Date(), t) }) : t('還沒校正過')}
          </p>
          <div className="form-actions">
            <button type="button" className="btn primary" data-k="anchor-save" disabled={!Number.isFinite(usdVal) || usdVal < 0 || saving} onClick={() => void save()}>
              {t('校正')}
            </button>
            {apiBotId ? (
              <button type="button" className="btn" data-k="goto-api-bot" onClick={() => { setOpen(false); selectBot(apiBotId) }}>
                {t('到 claude-api bot')}
              </button>
            ) : null}
          </div>
        </div>
      </Modal>
    </>
  )
}
