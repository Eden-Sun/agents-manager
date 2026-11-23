import { useState, type ReactNode } from 'react'
import { useShallow } from 'zustand/react/shallow'
import type { Bot } from '../api/types'
import { clusterIsPaused, groupClusterItems, sortClusterEntries, type ExpertClusterItem } from '../lib/expertClusters'
import { t, useT } from '../lib/i18n'
import { botLamp, useStore } from '../store/store'
import { ExpertClusterRow } from './ExpertClusterRow'
import { ExpertSectionHeader } from './ExpertBadge'
import { NoDispatchIcon } from './Icons'

function readFlag(key: string, fallback: boolean): boolean {
  try {
    const v = window.localStorage.getItem(key)
    return v === null ? fallback : v === '1'
  } catch {
    return fallback
  }
}

function writeFlag(key: string, on: boolean) {
  try {
    window.localStorage.setItem(key, on ? '1' : '0')
  } catch {
  }
}

export function ExpertExecutorSection({
  projectId,
  title,
  abbr,
  items,
  query,
  renderMember,
}: {
  projectId: string
  title: string
  abbr?: string
  items: readonly Bot[]
  query: string
  renderMember: (b: Bot) => ReactNode
}) {
  useT()
  const sectionKey = `agm.expertSection.${projectId}.executor`
  const pausedKey = `agm.expertPaused.${projectId}`
  const [open, setOpen] = useState(() => readFlag(sectionKey, true))
  const [pausedOpen, setPausedOpen] = useState(() => readFlag(pausedKey, false))

  const ids = items.map((b) => b.id)
  const lamps = useStore(useShallow((s) => ids.map((id) => botLamp(s, id))))
  const lampsById = Object.fromEntries(ids.map((id, i) => [id, lamps[i]]))

  const groups = groupClusterItems(items)
  const clusters = groups.filter((e) => e.kind === 'cluster')
  const hasLive = clusters.some((e) => e.kind === 'cluster' && !clusterIsPaused(e.members))
  const hasPaused = clusters.some((e) => e.kind === 'cluster' && clusterIsPaused(e.members))
  const fold = hasLive && hasPaused
  const isPaused = (e: ExpertClusterItem<Bot>) => e.kind === 'cluster' && clusterIsPaused(e.members)
  const sorted = sortClusterEntries(groups, lampsById)
  const main = fold ? sorted.filter((e) => !isPaused(e)) : sorted
  const paused = fold ? sorted.filter(isPaused) : []
  const pausedBots = paused.reduce((n, e) => n + (e.kind === 'cluster' ? e.members.length : 0), 0)

  const expanded = open || Boolean(query)
  const foldOpen = pausedOpen || Boolean(query)

  const toggleSection = () => {
    const next = !open
    setOpen(next)
    writeFlag(sectionKey, next)
  }
  const togglePaused = () => {
    const next = !pausedOpen
    setPausedOpen(next)
    writeFlag(pausedKey, next)
  }

  const renderEntry = (entry: ExpertClusterItem<Bot>) =>
    entry.kind === 'single' ? (
      renderMember(entry.item)
    ) : (
      <ExpertClusterRow key={`cluster:${entry.name}`} projectId={projectId} name={entry.name} members={entry.members}>
        {entry.members.map(renderMember)}
      </ExpertClusterRow>
    )

  return (
    <>
      <ExpertSectionHeader title={title} abbr={abbr} collapsible open={expanded} onToggle={toggleSection} lamps={lamps} />
      {expanded ? (
        <>
          {main.map(renderEntry)}
          {paused.length > 0 ? (
            <>
              <button type="button" className="bot-cluster-paused-fold" aria-expanded={foldOpen} onClick={togglePaused}>
                <span className="bot-cluster-caret" aria-hidden="true">{foldOpen ? '▾' : '▸'}</span>
                <NoDispatchIcon />
                <span className="bot-cluster-paused-label">
                  {t('已停派（{clusters} 群集 · {bots} 顆）', { clusters: paused.length, bots: pausedBots })}
                </span>
              </button>
              {foldOpen ? paused.map(renderEntry) : null}
            </>
          ) : null}
        </>
      ) : null}
    </>
  )
}
