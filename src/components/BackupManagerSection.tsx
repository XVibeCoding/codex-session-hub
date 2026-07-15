import {
  FolderOpen,
  HardDrive,
  Pin,
  PinOff,
  RefreshCw,
  RotateCcw,
  ShieldCheck,
  Trash2,
} from 'lucide-react'
import type { BackupEntry, BackupSummary } from '../app-types'

type BackupManagerSectionProps = {
  summary: BackupSummary | null
  loading: boolean
  busy: boolean
  actionPath: string | null
  rollbackDisabled: boolean
  historicalRestoreDisabled: boolean
  rollbackLabel: string
  onRollbackLatest: () => void
  onRefresh: () => void
  onOpenFolder: () => void
  onCleanup: (includeLegacy: boolean) => void
  onRetain: (entry: BackupEntry) => void
  onRestore: (entry: BackupEntry) => void
}

function formatBytes(bytes: number) {
  if (bytes < 1024 * 1024) return `${Math.max(0, bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

function formatBackupTime(value: string) {
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return '时间未知'
  return date.toLocaleString('zh-CN', {
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  })
}

function backupKindLabel(entry: BackupEntry) {
  if (entry.kind === 'manual') return '手动保留'
  if (entry.kind === 'restoreSafety') return '恢复前快照'
  return '自动快照'
}

function backupStatusLabel(entry: BackupEntry) {
  if (entry.protected) return '操作保护中'
  if (entry.pinned) return '已锁定'
  if (entry.status === 'legacy') return '旧版不兼容'
  if (entry.status === 'corrupt') return '校验失败'
  if (entry.status === 'incomplete') return '未完成'
  return '可回滚'
}

export function BackupManagerSection({
  summary,
  loading,
  busy,
  actionPath,
  rollbackDisabled,
  historicalRestoreDisabled,
  rollbackLabel,
  onRollbackLatest,
  onRefresh,
  onOpenFolder,
  onCleanup,
  onRetain,
  onRestore,
}: BackupManagerSectionProps) {
  const entries = summary?.entries ?? []
  const activeCount = summary?.restorableCount ?? 0
  const totalBytes = summary?.totalBytes ?? 0
  const reviewEntries = entries.filter(entry => entry.status === 'legacy' || entry.status === 'corrupt')
  const reviewBytes = reviewEntries.reduce((total, entry) => total + entry.sizeBytes, 0)

  return (
    <section className="detail-section backup-manager-section">
      <div className="section-title-row">
        <h3><HardDrive size={16} />备份与回滚</h3>
        <div className="backup-toolbar">
          <button className="icon-button compact" type="button" title="刷新备份列表" aria-label="刷新备份列表" onClick={onRefresh} disabled={loading || busy}>
            <RefreshCw size={15} className={loading ? 'spin' : undefined} />
          </button>
          <button className="icon-button compact" type="button" title="打开备份目录" aria-label="打开备份目录" onClick={onOpenFolder}>
            <FolderOpen size={15} />
          </button>
        </div>
      </div>

      <div className="backup-overview">
        <span><ShieldCheck size={17} /><strong>{activeCount}</strong> 个可回滚备份</span>
        <span>{formatBytes(totalBytes)}</span>
      </div>
      <p className="backup-policy">
        自动保留最近 {summary?.automaticLimit ?? 5} 个，容量上限 {formatBytes(summary?.capacityLimitBytes ?? 250 * 1024 * 1024)}，始终保留至少 {summary?.minimumAutomatic ?? 2} 个健康回滚点。
      </p>

      <button className="text-button danger backup-latest-action" type="button" onClick={onRollbackLatest} disabled={rollbackDisabled || busy}>
        <RotateCcw size={14} />{rollbackLabel}
      </button>

      <details className="backup-history">
        <summary>历史备份 <span>{entries.length}</span></summary>
        <div className="backup-history-list">
          {loading ? <p className="backup-empty">正在读取备份...</p> : null}
          {!loading && entries.length === 0 ? <p className="backup-empty">尚未创建修复备份。</p> : null}
          {!loading ? entries.map(entry => {
            const entryBusy = actionPath === entry.path
            const canRestore = entry.restorable && !entry.protected && !historicalRestoreDisabled
            const canRetain = entry.restorable && !entry.protected
            return (
              <div className="backup-row" key={entry.path}>
                <div className="backup-row-main">
                  <strong>{formatBackupTime(entry.createdAt)}</strong>
                  <span>{backupKindLabel(entry)} · {entry.provider || 'Provider 未记录'} · {formatBytes(entry.sizeBytes)}</span>
                  <small title={entry.path}>{entry.path}</small>
                </div>
                <div className="backup-row-actions">
                  <em className={`backup-status ${entry.status}`}>{backupStatusLabel(entry)}</em>
                  <button
                    className="icon-button compact"
                    type="button"
                    title={entry.pinned ? '恢复自动管理' : '锁定保留'}
                    aria-label={entry.pinned ? '恢复自动管理' : '锁定保留'}
                    onClick={() => onRetain(entry)}
                    disabled={!canRetain || busy || entryBusy}
                  >
                    {entry.pinned ? <PinOff size={15} /> : <Pin size={15} />}
                  </button>
                  <button
                    className="text-button backup-restore-button"
                    type="button"
                    onClick={() => onRestore(entry)}
                    disabled={!canRestore || busy || entryBusy}
                  >
                    <RotateCcw size={14} />恢复
                  </button>
                </div>
              </div>
            )
          }) : null}
        </div>
      </details>

      {reviewEntries.length ? (
        <div className="backup-warning">
          <span>发现 {reviewEntries.length} 个旧版或损坏备份，占用 {formatBytes(reviewBytes)}。当前版本不会自动删除。</span>
          <button className="text-button danger" type="button" onClick={() => onCleanup(true)} disabled={busy || loading}>
            <Trash2 size={14} />确认清理
          </button>
        </div>
      ) : null}

      {summary?.warnings.length ? (
        <p className="backup-read-warning">部分备份无法完整读取，请查看执行日志后再决定是否清理。</p>
      ) : null}

      <div className="backup-footer-actions">
        <button className="text-button" type="button" onClick={() => onCleanup(false)} disabled={busy || loading}>
          <Trash2 size={14} />清理可安全删除的旧备份
        </button>
      </div>
    </section>
  )
}
