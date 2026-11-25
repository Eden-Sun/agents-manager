import type { ScheduleRunner } from '../api/schedules'
import { effortLabel } from '../api/types'
import { agyEffort, shortModel } from '../lib/shortModel'
import { t } from '../lib/i18n'
import './runnerTag.css'

export function runnerLabel(r: ScheduleRunner): { model: string; effort: string } {
  const effort = r.effort ?? agyEffort(r.kind, r.model)
  return { model: shortModel(r.kind, r.model) ?? t('CLI 預設'), effort: effort ? effortLabel(effort) : '' }
}

export function RunnerTag({ runner }: { runner?: ScheduleRunner | null }) {
  if (!runner) return null
  const { model, effort } = runnerLabel(runner)
  const title = [
    t('Bot：{name}', { name: runner.bot_name }),
    t('模型：{model}', { model: runner.model ?? t('（CLI 預設）') }),
    effort,
    runner.identity ? t('身份：{id}', { id: runner.identity }) : '',
    runner.source === 'bot' ? t('取自 Bot 設定') : t('取自執行中的 run'),
  ].filter(Boolean).join(' · ')
  return (
    <p className="schedule-runner" title={title} data-runner-kind={runner.kind}>
      <span className="schedule-runner-bot">{runner.bot_name}</span>
      <span className="model-tag schedule-runner-model">
        {runner.kind} {model}
        {effort ? <span className="model-tag-extra">{effort}</span> : null}
      </span>
    </p>
  )
}
