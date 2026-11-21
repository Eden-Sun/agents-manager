import { AgmReviewBox } from './AgmReviewBox'
import { UpdateChangelog } from './UpdateChangelog'

export function UpstreamChangelogBox({ kind, host, from, to, refreshKey = 0 }: { kind: 'agy' | 'grok'; host: string; from: string | null; to: string; refreshKey?: number }) {
  return (
    <div className="update-split">
      <UpdateChangelog kind={kind} host={host} from={from} to={to} autoTranslate />
      <AgmReviewBox kind={kind} host={host} from={from} to={to} refreshKey={refreshKey} />
    </div>
  )
}
