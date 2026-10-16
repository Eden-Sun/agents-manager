import { Fragment, useId, useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import type { Message } from '../api/types'
import { childNoticeLabel, groupChildNotices } from '../lib/childNotice'
import type { ChildNoticeRow } from '../lib/childNotice'
import { msgTimeLabel, msgTimeTitle } from '../lib/msgTime'
import { useLocalDay } from '../hooks/useLocalDay'
import './childNoticeMessages.css'

type NoticeGroup = Extract<ChildNoticeRow, { kind: 'notices' }>
function ChildNoticeGroup({ row, renderMessage }: {
  row: NoticeGroup; renderMessage: (msg: Message, expanded?: boolean) => ReactNode
}) {
  const [open, setOpen] = useState(false)
  const id = useId()
  const today = useLocalDay()
  const at = row.messages.at(-1)!.created_at
  const label = childNoticeLabel(row.notices)
  return <section className="child-notice-group">
    <button type="button" className="child-notice-toggle" aria-expanded={open} aria-controls={id} onClick={() => setOpen(!open)} title={label}>
      <span className="child-notice-label">{label}</span>
      <time dateTime={at} title={msgTimeTitle(at)}>{msgTimeLabel(at, today)}</time>
      <span aria-hidden="true">{open ? '▾' : '▸'}</span>
    </button>
    {open ? <div id={id} className="child-notice-content">
      {row.messages.map((msg) => <Fragment key={msg.id}>{renderMessage(msg, true)}</Fragment>)}
    </div> : null}
  </section>
}

/** parent 對通知的回覆（含串流）照一般訊息顯示，只有通知本身收成一行。 */
export function ChildNoticeMessages({ messages, renderMessage, liveReply }: {
  messages: readonly Message[]; renderMessage: (msg: Message, expanded?: boolean) => ReactNode; liveReply?: ReactNode
}) {
  const rows = useMemo(() => groupChildNotices(messages), [messages])
  return <>
    {rows.map((row) => row.kind === 'message'
      ? <Fragment key={row.message.id}>{renderMessage(row.message)}</Fragment>
      : <ChildNoticeGroup key={row.id} row={row} renderMessage={renderMessage} />)}
    {liveReply}
  </>
}
