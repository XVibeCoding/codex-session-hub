import { memo, useEffect, useRef } from 'react'
import {
  Archive,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  CircleAlert,
  Copy,
  FileJson,
  Folder,
  FolderOpen,
  History,
} from 'lucide-react'
import type { LocalSession, SessionGroup } from '../app-types'

type MixedCheckboxProps = {
  checked: boolean
  mixed?: boolean
  label: string
  onChange: () => void
}

function MixedCheckbox({ checked, mixed = false, label, onChange }: MixedCheckboxProps) {
  const ref = useRef<HTMLInputElement>(null)

  useEffect(() => {
    if (ref.current) ref.current.indeterminate = mixed
  }, [mixed])

  return (
    <input
      ref={ref}
      className="selection-checkbox"
      type="checkbox"
      checked={checked}
      aria-label={label}
      onChange={onChange}
    />
  )
}

function formatRelativeTime(timestamp: number) {
  if (!timestamp) return '时间未知'
  const milliseconds = timestamp > 10_000_000_000 ? timestamp : timestamp * 1000
  const delta = Math.max(0, Date.now() - milliseconds)
  const minutes = Math.floor(delta / 60_000)
  if (minutes < 1) return '刚刚'
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.floor(hours / 24)
  if (days < 7) return `${days} 天前`
  return new Date(milliseconds).toLocaleDateString('zh-CN', { month: 'numeric', day: 'numeric' })
}

function compactSessionId(id: string) {
  if (id.length <= 22) return id
  return `${id.slice(0, 10)}...${id.slice(-8)}`
}

function SessionStatus({ session }: { session: LocalSession }) {
  if (session.status === 'visible') {
    return <span className="session-status visible"><CheckCircle2 size={14} />当前可见</span>
  }
  if (session.status === 'needsConfirmation') {
    return <span className="session-status attention"><CircleAlert size={14} />需确认路径</span>
  }
  if (session.status === 'archived') {
    return <span className="session-status archived"><Archive size={14} />已归档</span>
  }
  return <span className="session-status recoverable"><History size={14} />可恢复</span>
}

type SessionRowProps = {
  session: LocalSession
  selected: boolean
  onToggle: (id: string) => void
  onCopyId: (session: LocalSession) => void
  onRevealRollout: (session: LocalSession) => void
}

const SessionRow = memo(function SessionRow({
  session,
  selected,
  onToggle,
  onCopyId,
  onRevealRollout,
}: SessionRowProps) {
  return (
    <div className={`session-row${selected ? ' selected' : ''}`}>
      <MixedCheckbox
        checked={selected}
        label={`选择会话 ${session.title}`}
        onChange={() => onToggle(session.id)}
      />
      <div className="session-main">
        <div className="session-copy">
          <strong title={session.title}>{session.title}</strong>
          <span title={session.id}>会话 ID：{compactSessionId(session.id)}</span>
        </div>
        <div className="session-actions">
          <button
            type="button"
            title="复制完整会话 ID"
            aria-label={`复制会话 ID ${session.id}`}
            onClick={() => onCopyId(session)}
          >
            <Copy size={15} />
          </button>
          <button
            type="button"
            title={session.rolloutPath ? '在文件管理器中定位 rollout JSONL' : '未找到 rollout JSONL'}
            aria-label={`定位会话 ${session.title} 的 rollout JSONL`}
            disabled={!session.rolloutPath}
            onClick={() => onRevealRollout(session)}
          >
            <FileJson size={16} />
          </button>
        </div>
      </div>
      <span className="session-provider" title={session.internal ? 'Codex 内部会话' : session.originProvider}>
        {session.originProvider}{session.internal ? ' · 内部' : ''}
      </span>
      <SessionStatus session={session} />
      <time>{formatRelativeTime(session.updatedAt)}</time>
    </div>
  )
})

type ProjectGroupProps = {
  group: SessionGroup
  open: boolean
  selectedIds: Set<string>
  onToggleOpen: (key: string) => void
  onToggleGroup: (group: SessionGroup) => void
  onToggleSession: (id: string) => void
  onOpenProject: (group: SessionGroup) => void
  onCopySessionId: (session: LocalSession) => void
  onRevealRollout: (session: LocalSession) => void
}

const ProjectGroup = memo(function ProjectGroup({
  group,
  open,
  selectedIds,
  onToggleOpen,
  onToggleGroup,
  onToggleSession,
  onOpenProject,
  onCopySessionId,
  onRevealRollout,
}: ProjectGroupProps) {
  const selectedCount = group.sessions.reduce(
    (total, session) => total + Number(selectedIds.has(session.id)),
    0,
  )
  const allSelected = selectedCount === group.sessions.length

  return (
    <section className="project-group">
      <div className="project-header">
        <MixedCheckbox
          checked={allSelected}
          mixed={selectedCount > 0 && !allSelected}
          label={`选择项目 ${group.name} 的全部会话`}
          onChange={() => onToggleGroup(group)}
        />
        <button
          className="project-chevron"
          type="button"
          aria-expanded={open}
          aria-label={`${open ? '收起' : '展开'}项目 ${group.name}`}
          onClick={() => onToggleOpen(group.key)}
        >
          {open ? <ChevronDown size={17} /> : <ChevronRight size={17} />}
        </button>
        <button
          className="project-folder-button"
          type="button"
          title={`在文件管理器中打开：${group.path}`}
          aria-label={`打开项目文件夹 ${group.name}`}
          onClick={() => onOpenProject(group)}
        >
          <FolderOpen size={18} />
        </button>
        <button
          className="project-identity"
          type="button"
          title={group.path}
          aria-expanded={open}
          onClick={() => onToggleOpen(group.key)}
        >
          <strong>{group.name}</strong>
        </button>
        <span className="project-count">{group.sessions.length} 个会话</span>
        <time>{formatRelativeTime(group.latest)}</time>
      </div>
      {open ? (
        <div className="session-rows">
          {group.sessions.map(session => (
            <SessionRow
              key={session.id}
              session={session}
              selected={selectedIds.has(session.id)}
              onToggle={onToggleSession}
              onCopyId={onCopySessionId}
              onRevealRollout={onRevealRollout}
            />
          ))}
        </div>
      ) : null}
    </section>
  )
})

type SessionExplorerProps = {
  groups: SessionGroup[]
  expandedGroups: Set<string>
  selectedIds: Set<string>
  forceOpen: boolean
  onToggleGroupOpen: (key: string) => void
  onToggleGroupSelection: (group: SessionGroup) => void
  onToggleSession: (id: string) => void
  onOpenProject: (group: SessionGroup) => void
  onCopySessionId: (session: LocalSession) => void
  onRevealRollout: (session: LocalSession) => void
}

export function SessionExplorer({
  groups,
  expandedGroups,
  selectedIds,
  forceOpen,
  onToggleGroupOpen,
  onToggleGroupSelection,
  onToggleSession,
  onOpenProject,
  onCopySessionId,
  onRevealRollout,
}: SessionExplorerProps) {
  if (groups.length === 0) {
    return (
      <div className="empty-state">
        <Folder size={30} />
        <strong>没有匹配的本地会话</strong>
        <span>清除搜索条件后再试。</span>
      </div>
    )
  }

  return (
    <div className="session-explorer" aria-label="本地 Codex 会话">
      {groups.map(group => (
        <ProjectGroup
          key={group.key}
          group={group}
          open={forceOpen || expandedGroups.has(group.key)}
          selectedIds={selectedIds}
          onToggleOpen={onToggleGroupOpen}
          onToggleGroup={onToggleGroupSelection}
          onToggleSession={onToggleSession}
          onOpenProject={onOpenProject}
          onCopySessionId={onCopySessionId}
          onRevealRollout={onRevealRollout}
        />
      ))}
    </div>
  )
}
