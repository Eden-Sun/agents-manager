import { useEffect, useMemo, useRef, useState } from 'react'
import type { RefObject } from 'react'
import { useShallow } from 'zustand/react/shallow'
import { parseMentions } from '../api/mentions'
import type { Bot, GroupMessage } from '../api/types'
import { useScrollTail } from '../hooks/useScrollTail'
import { attachCommandOf, botLamp, composerState, groupComposerState, projectHostName, useStore } from '../store/store'
import { useEnterToSend } from '../hooks/useEnterToSend'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { useComposerFocus } from '../hooks/useComposerFocus'
import { AttachButton } from './AttachButton'
import { AttachPicker, AttachTray, DropVeil, isImageFile, useAttachments, useDropTarget } from './Attachments'
import { ProjectNameField } from './ProjectNameField'
import { Bubble, EmptyState, JumpToBottom, KIND_TITLE, LiveReplyBubble, LoadEarlier } from './ChatPanel'
import { HostBadge } from './HostsPanel'
import { useShelfSink } from './ImageShelf'
import { IssuesBar } from './IssuesBar'
import { KindIcon } from './KindTag'
import { MemBadge } from './MemBadge'
import { QuotaStrip } from './QuotaStrip'
import { UnreadChip } from './UnreadChip'
import { ToolsHint, ToolsHintIcon } from './Tools'
import type { BotKind } from '../api/types'
import { LAMP_LABEL, StatusLamp } from './StatusLamp'

/**
 * SPEC §13 project group chat: one Project = one group. The timeline is every member bot's
 * conversation merged (by message id); `@<bot>` / `@all` in the composer picks recipients.
 */

const MENTION_RE = /(^|[^\p{L}\p{N}_])@([^\s@,:;?!。，、！？()（）[\]{}<>"']+)/gu

/** Strip @mentions so chip selection can rewrite the recipient prefix. */
function stripMentions(text: string): string {
  return text.replace(MENTION_RE, '$1').replace(/[ \t]{2,}/g, ' ').replace(/^\s+/, '')
}

function hasAllMention(text: string): boolean {
  for (const m of text.matchAll(MENTION_RE)) {
    if (m[2].toLowerCase() === 'all') return true
  }
  return false
}

function applyRecipients(text: string, mode: 'all' | string[]): string {
  const body = stripMentions(text)
  if (mode === 'all') return body ? `@all ${body}` : '@all '
  if (mode.length === 0) return body
  const prefix = mode.map((n) => `@${n}`).join(' ')
  return body ? `${prefix} ${body}` : `${prefix} `
}

type Row =
  | { key: string; kind: 'user'; msg: GroupMessage; targets: string[] }
  | { key: string; kind: 'bot'; msg: GroupMessage }

/**
 * §13.1: a group send writes one user Message per recipient (same `group_id`); fold those
 * copies into one row that lists the recipients. Everything else is one row per message.
 */
function foldRows(list: GroupMessage[]): Row[] {
  const rows: Row[] = []
  const byGroup = new Map<string, Row & { kind: 'user' }>()
  for (const m of list) {
    if (m.role === 'user') {
      if (m.group_id) {
        const prev = byGroup.get(m.group_id)
        if (prev) {
          if (!prev.targets.includes(m.bot_name)) prev.targets.push(m.bot_name)
          continue
        }
        const row: Row & { kind: 'user' } = { key: m.id, kind: 'user', msg: m, targets: [m.bot_name] }
        byGroup.set(m.group_id, row)
        rows.push(row)
      } else {
        rows.push({ key: m.id, kind: 'user', msg: m, targets: [m.bot_name] })
      }
    } else {
      rows.push({ key: m.id, kind: 'bot', msg: m })
    }
  }
  return rows
}

/** Icons + a count, nothing else: who is in the group is the sidebar's job. */
function MemberStrip({ projectId }: { projectId: string }) {
  const members = useStore(useShallow((s) => s.bots.filter((b) => b.project_id === projectId)))
  const selectBot = useStore((s) => s.selectBot)
  return (
    <div className="members" role="list" aria-label="群組成員">
      {members.map((b) => (
        <MemberChip key={b.id} bot={b} onOpen={() => selectBot(b.id)} />
      ))}
      {members.length === 0 ? (
        <span className="hint">（尚無 Bot）</span>
      ) : (
        <span className="members-count">{members.length} 個成員</span>
      )}
    </div>
  )
}

/**
 * Icon only. The names used to be spelled out here, which cost the header ~90px per member
 * to repeat what the sidebar already lists; the count lives in `.main-status` next to the
 * strip, and the name is one hover away.
 */
function MemberChip({ bot, onOpen }: { bot: Bot; onOpen: () => void }) {
  const lamp = useStore((s) => botLamp(s, bot.id))
  return (
    <button
      type="button"
      className={`member member-icon ${bot.kind}`}
      role="listitem"
      title={`${bot.name}：${LAMP_LABEL[lamp]}（點擊開啟單獨對話）`}
      aria-label={`${bot.name}：${LAMP_LABEL[lamp]}`}
      onClick={onOpen}
    >
      <KindIcon kind={bot.kind} />
      <StatusLamp lamp={lamp} title={`${bot.name}：${LAMP_LABEL[lamp]}`} />
    </button>
  )
}

function GroupLiveBubbles({ projectId }: { projectId: string }) {
  const typing = useStore(
    useShallow((s) =>
      s.bots
        .filter((b) => b.project_id === projectId)
        .filter((b) => s.runs[b.id]?.agent_status === 'working' || composerState(s, b.id).inFlightTurnId !== null),
    ),
  )
  return typing.map((t) => (
    <LiveReplyBubble
      key={`typing-${t.id}`}
      botId={t.id}
      kind={t.kind}
      from={`${t.name} · ${KIND_TITLE[t.kind]}`}
      abandon
    />
  ))
}

function GroupMessageList({ projectId }: { projectId: string }) {
  const messages = useStore((s) => s.groupMessages[projectId])
  const loaded = useStore((s) => Boolean(s.loadedProjects[projectId]))
  const kinds = useStore(useShallow((s) => Object.fromEntries(s.bots.map((b) => [b.id, b.kind]))))
  const rows = useMemo(() => foldRows(messages ?? []), [messages])
  const loadEarlier = useStore((s) => s.loadEarlierGroupMessages)
  const tail = useScrollTail([rows])

  return (
    <div className="msg-list-wrap">
    <div className="msg-list group" ref={tail.ref} onScroll={tail.onScroll}>
      <LoadEarlier id={projectId} onLoad={loadEarlier} />
      {rows.length === 0 ? (
        <EmptyState
          loading={!loaded}
          title={loaded ? '開始交代第一個任務' : undefined}
          icon={loaded ? '✦' : undefined}
        >
          {loaded ? '在下方以 @bot 或 @all 對成員送出第一則訊息。' : '載入群組訊息中…'}
        </EmptyState>
      ) : (
        rows.map((r) =>
          r.kind === 'user' ? (
            <Bubble
              key={r.key}
              msg={r.msg}
              from={`你 → ${r.targets.join(', ')}`}
              fromClassName="msg-targets"
              fromTitle="這則訊息送給了這些 Bot"
            />
          ) : (
            <Bubble
              key={r.key}
              msg={r.msg}
              kind={kinds[r.msg.bot_id]}
              from={`${r.msg.bot_name}${kinds[r.msg.bot_id] ? ` · ${KIND_TITLE[kinds[r.msg.bot_id]]}` : ''}`}
              fromClassName="msg-speaker"
            />
          ),
        )
      )}
      <GroupLiveBubbles projectId={projectId} />
    </div>
    <JumpToBottom show={!tail.atBottom && rows.length > 0} onClick={tail.toBottom} />
    </div>
  )
}

interface Candidate {
  name: string
  label: string
  kind?: Bot['kind']
}

/** The `@` token under the caret, if the caret sits at the end of one. */
function mentionAtCaret(text: string, caret: number): { start: number; query: string } | null {
  const head = text.slice(0, caret)
  const m = /(^|[^\p{L}\p{N}_])@([A-Za-z0-9_-]*)$/u.exec(head)
  if (!m) return null
  return { start: caret - m[2].length - 1, query: m[2] }
}

function GroupComposer({
  projectId,
  inputRef,
  files,
}: {
  projectId: string
  inputRef: RefObject<HTMLTextAreaElement | null>
  /** Owned by `GroupChatPanel` so a drop anywhere in the chat area lands here. */
  files: ReturnType<typeof useAttachments>
}) {
  // `groupComposerState` builds a fresh object (with an array) every call; flatten it to
  // primitives so the shallow comparison is stable.
  const state = useStore(
    useShallow((s) => {
      const g = groupComposerState(s, projectId)
      return { disabled: g.disabled, reason: g.reason, sendableKey: g.sendable.join(',') }
    }),
  )
  const sendable = useMemo(() => state.sendableKey.split(',').filter(Boolean), [state.sendableKey])
  const members = useStore(useShallow((s) => s.bots.filter((b) => b.project_id === projectId)))
  const sendGroupChat = useStore((s) => s.sendGroupChat)
  const notify = useStore((s) => s.notify)
  // v4.0: draft per group in the store (localStorage-backed); the `@` popup state stays local.
  const draftKey = `group:${projectId}` as const
  const text = useStore((s) => s.drafts[draftKey] ?? '')
  const setDraft = useStore((s) => s.setDraft)
  const setDraftCursor = useStore((s) => s.setDraftCursor)
  const setText = (v: string) => setDraft(draftKey, v)
  const caret = useStore((s) => {
    const value = s.drafts[draftKey] ?? ''
    const saved = s.draftCursors[draftKey]?.start ?? value.length
    return Math.max(0, Math.min(value.length, saved))
  })
  const [sending, setSending] = useState(false)
  const [popOpen, setPopOpen] = useState(true)
  const [active, setActive] = useState(0)
  const ref = inputRef

  const loaded = useStore((s) => Boolean(s.loadedProjects[projectId]))
  const empty = useStore((s) => (s.groupMessages[projectId]?.length ?? 0) === 0)
  const focusEmpty = loaded && empty
  const phone = useMediaQuery(PHONE_QUERY)

  // 手機不自動 focus：鍵盤會擋住視線，要打字自己點（同 ChatPanel）。
  useComposerFocus({ draftKey, ref, forceFocus: focusEmpty && !phone, autoFocus: !phone })

  useEffect(() => {
    const el = ref.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${Math.min(200, el.scrollHeight)}px`
  }, [text, ref])

  const mention = mentionAtCaret(text, caret)
  const candidates: Candidate[] = useMemo(() => {
    if (!mention) return []
    const q = mention.query.toLowerCase()
    const all: Candidate[] = [
      { name: 'all', label: `所有 Bot（${members.length}）` },
      ...members.map((b) => ({ name: b.name, label: b.kind, kind: b.kind })),
    ]
    return all.filter((c) => c.name.toLowerCase().startsWith(q))
  }, [mention, members])
  const showPop = popOpen && candidates.length > 0
  const activeIdx = Math.min(active, Math.max(0, candidates.length - 1))

  const targets = useMemo(() => parseMentions(text, members), [text, members])
  const allSelected = hasAllMention(text)
  const selectedNames = useMemo(() => new Set(targets.map((t) => t.name)), [targets])
  const skippedNow = targets.filter((b) => !sendable.includes(b.id))

  const writeDraft = (next: string) => {
    setText(next)
    const pos = next.length
    setDraftCursor(draftKey, pos)
    requestAnimationFrame(() => {
      const el = ref.current
      if (!el) return
      el.focus()
      el.setSelectionRange(pos, pos)
    })
  }

  const toggleAll = () => {
    if (allSelected) writeDraft(stripMentions(text))
    else writeDraft(applyRecipients(text, 'all'))
  }

  const toggleBot = (name: string) => {
    if (allSelected) {
      writeDraft(applyRecipients(text, [name]))
      return
    }
    const next = members.map((b) => b.name).filter((n) => (n === name ? !selectedNames.has(n) : selectedNames.has(n)))
    writeDraft(applyRecipients(text, next))
  }

  const pick = (c: Candidate) => {
    if (!mention) return
    const before = text.slice(0, mention.start)
    const after = text.slice(caret)
    const insert = `@${c.name} `
    const pos = before.length + insert.length
    setText(before + insert + after)
    setDraftCursor(draftKey, pos)
    setPopOpen(true)
    setActive(0)
    // Put the caret right after the completion once React has flushed the new value.
    requestAnimationFrame(() => {
      const el = ref.current
      if (!el) return
      el.focus()
      el.setSelectionRange(pos, pos)
    })
  }

  const submit = () => {
    const body = text.trim()
    // Unlike a bot chat, a group send always needs text: the recipients come from it.
    if (!body || targets.length === 0) return
    // 打字沒被鎖，送不出去就講原因，別默默吃掉 Enter。
    if (state.disabled) {
      notify('error', state.reason || '目前無法送出訊息')
      return
    }
    if (sending || files.uploading) return
    setSending(true)
    void sendGroupChat(projectId, body, files.ids).then((res) => {
      setSending(false)
      if (res) {
        setText('')
        files.clear()
      }
    })
  }
  const enterToSend = useEnterToSend()

  const syncCaret = () => {
    const el = ref.current
    if (el) setDraftCursor(draftKey, el.selectionStart ?? el.value.length, el.selectionEnd ?? el.selectionStart ?? el.value.length)
  }

  return (
    <div className="composer group-composer">
      {state.disabled && state.reason ? (
        <div className="composer-lock" role="status">
          {/* 同 `ChatPanel`：emoji 吃不到 `color`，拿掉。 */}
          <span>{state.reason}</span>
        </div>
      ) : null}
      <div className="recipient-row" role="group" aria-label="收件者">
        <button
          type="button"
          className={`recipient-chip all${allSelected ? ' on' : ''}`}
          aria-pressed={allSelected}
          disabled={state.disabled || sending || members.length === 0}
          title={`送給全部 ${members.length} 個 bot`}
          onClick={toggleAll}
        >
          @all · {members.length} 個 bot
        </button>
        {members.map((b) => {
          const on = !allSelected && selectedNames.has(b.name)
          return (
            <button
              key={b.id}
              type="button"
              className={`recipient-chip ${b.kind}${on ? ' on' : ''}`}
              aria-pressed={on}
              disabled={state.disabled || sending}
              title={`送給 @${b.name}`}
              onClick={() => toggleBot(b.name)}
            >
              @{b.name}
            </button>
          )
        })}
      </div>
      <AttachTray items={files.items} onRemove={files.remove} disabled={sending} />
      <div className="composer-box">
        <AttachPicker onFiles={files.add} disabled={state.disabled || sending} />
        {showPop ? (
          <ul className="mention-pop" role="listbox" aria-label="選擇收件 Bot">
            {candidates.map((c, i) => (
              <li
                key={c.name}
                role="option"
                aria-selected={i === activeIdx}
                className={`mention-item${i === activeIdx ? ' active' : ''}`}
                onMouseDown={(e) => {
                  e.preventDefault()
                  pick(c)
                }}
                onMouseEnter={() => setActive(i)}
              >
                <span className={`bot-badge${c.kind ? ` ${c.kind}` : ' all'}`}>@{c.name}</span>
                <span className="mention-label">{c.label}</span>
              </li>
            ))}
          </ul>
        ) : null}
        <textarea
          ref={ref}
          value={text}
          /* 連線斷了也讓人繼續打（草稿會存），只是送不出去。 */
          disabled={sending}
          placeholder={state.disabled ? `${state.reason || '目前無法送出訊息'}——可以先打，恢復後再送` : '@bot 或 @all …'}
          title="以 @<bot> 或 @all 指定收件者；Enter 送出，Shift+Enter 換行"
          onChange={(e) => {
            setText(e.target.value)
            setDraftCursor(draftKey, e.target.selectionStart ?? e.target.value.length, e.target.selectionEnd ?? e.target.value.length)
            setPopOpen(true)
            setActive(0)
          }}
          onSelect={syncCaret}
          onClick={syncCaret}
          onBlur={syncCaret}
          onPaste={(e) => {
            const imgs = Array.from(e.clipboardData?.files ?? []).filter(isImageFile)
            if (imgs.length === 0) return
            e.preventDefault()
            files.add(imgs)
          }}
          onKeyUp={syncCaret}
          onKeyDown={(e) => {
            if (e.nativeEvent.isComposing) return
            if (showPop) {
              if (e.key === 'ArrowDown') {
                e.preventDefault()
                setActive((activeIdx + 1) % candidates.length)
                return
              }
              if (e.key === 'ArrowUp') {
                e.preventDefault()
                setActive((activeIdx - 1 + candidates.length) % candidates.length)
                return
              }
              if (e.key === 'Enter' || e.key === 'Tab') {
                e.preventDefault()
                pick(candidates[activeIdx])
                return
              }
              if (e.key === 'Escape') {
                e.preventDefault()
                setPopOpen(false)
                return
              }
            }
            if (enterToSend.enterSends && e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault()
              submit()
            }
          }}
          {...enterToSend.props}
        />
        <button
          type="button"
          className="send-btn"
          disabled={state.disabled || sending || files.uploading || !text.trim() || targets.length === 0}
          title={
            files.uploading
              ? '圖片上傳中…'
              : targets.length === 0
                ? '請選擇收件者（上方 chip 或 @mention）'
                : `送給 ${targets.map((t) => `@${t.name}`).join(', ')}`
          }
          onClick={submit}
        >
          {sending ? '送出中…' : files.uploading ? '上傳中…' : '送出'}
        </button>
      </div>
      {/* Only while there is something to say: no mention yet, or the resolved recipient list. */}
      {text.trim() && targets.length === 0 ? (
        <div className="composer-hint group-hint">
          <span className="mention-warn">請選擇上方收件者，或以 @&lt;bot 名稱&gt; / @all 指定</span>
        </div>
      ) : targets.length > 0 ? (
        <div className="composer-hint group-hint">
          <span className="group-targets">
            → {allSelected ? `@all（${members.length} 個 bot）` : targets.map((t) => `@${t.name}`).join(', ')}
            {skippedNow.length > 0 ? (
              <span className="mention-warn">
                {' '}
                ・ {skippedNow.map((t) => `@${t.name}`).join(', ')} 目前無法接收，會被略過（不會自動啟動）
              </span>
            ) : null}
          </span>
        </div>
      ) : null}
    </div>
  )
}

export function GroupChatPanel({ projectId, onOpenSidebar }: { projectId: string; onOpenSidebar: () => void }) {
  const project = useStore((s) => s.projects.find((p) => p.id === projectId) ?? null)
  const hostName = useStore((s) => projectHostName(s, projectId))
  const hostUp = useStore((s) => hostName === 'local' || (s.hosts.find((h) => h.name === hostName)?.connected ?? false))
  const members = useStore(useShallow((s) => s.bots.filter((b) => b.project_id === projectId)))
  const memberCount = members.length
  const memberKinds = useMemo(() => [...new Set(members.map((b) => b.kind))] as BotKind[], [members])
  const selectProject = useStore((s) => s.selectProject)
  const requestOpenBotSheet = useStore((s) => s.requestOpenBotSheet)
  const attachCommand = useStore((s) => attachCommandOf(s, projectId))
  const composerRef = useRef<HTMLTextAreaElement>(null)
  // Attachments are project-scoped, so any member can receive the upload; held here so a
  // drop anywhere in the group chat area is accepted.
  const files = useAttachments(members[0]?.id ?? null, projectId)
  const drop = useDropTarget(files.add, memberCount === 0)
  const [renaming, setRenaming] = useState(false)
  // 同 ChatPanel：讓右側圖片暫存區把「點一下」的圖片交給這個群組草稿。
  useShelfSink(files.add, memberCount > 0 ? `${project?.label ?? ''} 群組` : null)

  if (!project) {
    return (
      <>
        <div className="main-head">
          <button
            type="button"
            className="btn menu-btn icon-tip"
            onClick={onOpenSidebar}
            aria-label="開啟側邊欄"
            data-tip="開啟側邊欄"
          >
            ☰
          </button>
          <span className="main-status">找不到 Project</span>
        </div>
      </>
    )
  }

  return (
    <>
      <div className="main-head group-head">
        <button
          type="button"
          className="btn menu-btn icon-tip"
          onClick={onOpenSidebar}
          aria-label="開啟側邊欄"
          data-tip="開啟側邊欄"
        >
          ☰
        </button>
        <div className="main-title">
          <span className="group-icon" aria-hidden="true">
            ⌗
          </span>
          <ProjectNameField projectId={projectId} label={project.label} editing={renaming} onEditing={setRenaming} />
          <span className="group-tag">群組</span>
          <HostBadge host={hostName} connected={hostUp} />
        </div>
        <MemberStrip projectId={projectId} />
        <span className="spacer" />
        <ToolsHintIcon />
        <QuotaStrip host={hostName} />
        {/* 遠端才掛：本機的數字固定在左上角，這裡再放一次只是重複。 */}
        <MemBadge host={hostName} onlyRemote />
        <AttachButton command={attachCommand} compact />
        <div className="head-actions">
          <button type="button" className="mini-btn" onClick={() => selectProject(null)} title="回到單一 Bot 的對話">
            關閉群組
          </button>
        </div>
      </div>
      <UnreadChip />
      <ToolsHint focusHost={hostName} focusKinds={memberKinds} />
      {memberCount === 0 ? (
        <EmptyState
          title="此專案尚無 Bot"
          icon="＋"
          action={
            <button
              type="button"
              className="btn primary empty-add-btn"
              onClick={() => {
                onOpenSidebar()
                requestOpenBotSheet(projectId)
              }}
            >
              新增 Bot
            </button>
          }
        >
          為此專案建立第一個 Bot 後，即可在群組中交代任務。
        </EmptyState>
      ) : (
        <div className={`chat${drop.over ? ' dropping' : ''}`} {...drop.props}>
          {drop.over ? <DropVeil /> : null}
          <IssuesBar projectId={projectId} draftKey={`group:${projectId}`} inputRef={composerRef} />
          <GroupMessageList projectId={projectId} />
          <GroupComposer projectId={projectId} inputRef={composerRef} files={files} />
        </div>
      )}
    </>
  )
}
