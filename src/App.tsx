import {
  startTransition,
  useCallback,
  useDeferredValue,
  useEffect,
  useMemo,
  useState,
} from 'react'
import { Channel, invoke } from '@tauri-apps/api/core'
import { getVersion } from '@tauri-apps/api/app'
import {
  Activity,
  CheckCircle2,
  CircleAlert,
  Download,
  FolderSearch,
  Github,
  LoaderCircle,
  RefreshCw,
  Search,
  ShieldCheck,
  SlidersHorizontal,
  Sparkles,
  X,
} from 'lucide-react'
import type {
  BackupCleanupResult,
  BackupEntry,
  BackupSummary,
  BlockingProcess,
  CloseResult,
  DesktopRefresh,
  LocalSession,
  LogEntry,
  Preview,
  RecoveryMode,
  RecoveryPhase,
  RecoveryRange,
  RepairProgress,
  RepairResult,
  SessionGroup,
  VerifyResult,
} from './app-types'
import { AppUpdateDialog, openProjectRepository } from './components/AppUpdateDialog'
import { RecoveryDialog } from './components/RecoveryDialog'
import { SessionExplorer } from './components/SessionExplorer'
import { TechnicalDrawer } from './components/TechnicalDrawer'
import { useAppUpdater } from './hooks/useAppUpdater'

const LOG_STORAGE_KEY = 'codex-session-repair.logs.v1'
type SearchScope = 'project' | 'title'
const searchPlaceholders: Record<SearchScope, string> = {
  project: '模糊搜索项目名称或路径',
  title: '模糊搜索会话标题',
}

function isTauriDesktop() {
  return Boolean((window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__)
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauriDesktop()) throw new Error('此功能仅在 Tauri 桌面应用中可用')
  return invoke<T>(command, args)
}

async function copyText(value: string) {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(value)
    return
  }
  const textarea = document.createElement('textarea')
  textarea.value = value
  textarea.style.position = 'fixed'
  textarea.style.opacity = '0'
  document.body.appendChild(textarea)
  textarea.select()
  const copied = document.execCommand('copy')
  textarea.remove()
  if (!copied) throw new Error('系统剪贴板不可用')
}

function readLogs(): LogEntry[] {
  try {
    const value = JSON.parse(localStorage.getItem(LOG_STORAGE_KEY) ?? '[]')
    return Array.isArray(value) ? value.slice(0, 100) : []
  } catch {
    return []
  }
}

function createLog(tone: LogEntry['tone'], text: string): LogEntry {
  const now = new Date()
  return {
    id: `${now.getTime()}-${Math.random().toString(16).slice(2)}`,
    time: now.toLocaleTimeString('zh-CN', { hour12: false }),
    tone,
    text,
  }
}

function formatStorage(bytes: number) {
  if (bytes < 1024 * 1024) return `${Math.max(0, bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

function groupSessions(sessions: LocalSession[]): SessionGroup[] {
  const groups = new Map<string, SessionGroup>()
  for (const session of sessions) {
    const path = session.cwd.trim() || '未归类路径'
    const key = path.toLocaleLowerCase()
    const existing = groups.get(key)
    if (existing) {
      existing.sessions.push(session)
      existing.latest = Math.max(existing.latest, session.updatedAt)
      continue
    }
    groups.set(key, {
      key,
      name: session.projectName || '未归类项目',
      path,
      sessions: [session],
      latest: session.updatedAt,
    })
  }
  return [...groups.values()].sort((left, right) => right.latest - left.latest)
}

function fuzzyIncludes(value: string, query: string) {
  const normalizedValue = value.toLocaleLowerCase().replace(/\s+/g, ' ').trim()
  return query.split(/\s+/).every(token => normalizedValue.includes(token))
}

function sessionMatchesSearch(session: LocalSession, query: string, scope: SearchScope) {
  if (!query) return true
  if (scope === 'project') {
    return [session.projectName, session.cwd].some(value => fuzzyIncludes(value, query))
  }
  return fuzzyIncludes(session.title, query)
}

function uniqueProcessGroups(processes: BlockingProcess[]) {
  return [...new Map(
    processes.map(process => [process.applicationRootPid ?? process.identity.pid, process]),
  ).values()]
}

function isDatabaseBusy(error: unknown) {
  return /SQLITE_(?:BUSY|LOCKED|WRITE_CONFLICT)|database(?: table)? is locked|database is busy|resource busy/i.test(String(error))
}

function errorMessage(error: unknown) {
  const message = String(error)
    .replace(/^Error:\s*/i, '')
    .replace(/^IPC error:\s*/i, '')
  if (isDatabaseBusy(message)) {
    return '会话数据库正在写入，在线恢复暂时未获得写入窗口。可以直接重试；若持续发生，再安全关闭占用程序后继续。'
  }
  if (/active processes|SQLite resources are owned/i.test(message)) {
    return '当前修复操作需要独占会话数据库。请保存正在进行的工作，并在确认后使用关闭占用程序的兜底流程。'
  }
  if (/plan changed|latest preview|plan token/i.test(message)) {
    return '会话数据在检查后发生了变化，工具会在重试时重新扫描。'
  }
  if (/administrator|permission|access is denied/i.test(message)) {
    return 'Windows 拒绝了本次操作。通常先关闭相关 Codex 实例即可，不建议默认以管理员身份运行。'
  }
  if (/insufficient disk space for a safe recovery backup/i.test(message)) {
    return '系统盘剩余空间不足，无法先创建安全回滚点。工具尚未写入任何会话数据，请释放部分空间后重试。'
  }
  return message
}

export default function App() {
  const [appVersion, setAppVersion] = useState<string | null>(null)
  const [desktop, setDesktop] = useState<DesktopRefresh | null>(null)
  const [query, setQuery] = useState('')
  const deferredQuery = useDeferredValue(query.trim().toLocaleLowerCase())
  const [searchScope, setSearchScope] = useState<SearchScope>('title')
  const [selectedIds, setSelectedIds] = useState<Set<string>>(() => new Set())
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(() => new Set())
  const [scanning, setScanning] = useState(true)
  const [scanError, setScanError] = useState<string | null>(null)
  const [logs, setLogs] = useState<LogEntry[]>(readLogs)
  const [toast, setToast] = useState<string | null>(null)

  const [recoveryOpen, setRecoveryOpen] = useState(false)
  const [recoveryRange, setRecoveryRange] = useState<RecoveryRange>('all')
  const [recoveryMode, setRecoveryMode] = useState<RecoveryMode>('safe')
  const [recoveryPhase, setRecoveryPhase] = useState<RecoveryPhase>('idle')
  const [advancedOpen, setAdvancedOpen] = useState(false)
  const [activePreview, setActivePreview] = useState<Preview | null>(null)
  const [repairResult, setRepairResult] = useState<RepairResult | null>(null)
  const [repairProgress, setRepairProgress] = useState<RepairProgress | null>(null)
  const [recoveryError, setRecoveryError] = useState<string | null>(null)
  const [recoveryLockConflict, setRecoveryLockConflict] = useState(false)
  const [restartPath, setRestartPath] = useState<string | null>(null)
  const [technicalOpen, setTechnicalOpen] = useState(false)
  const [backups, setBackups] = useState<BackupSummary | null>(null)
  const [backupLoading, setBackupLoading] = useState(false)
  const [backupActionPath, setBackupActionPath] = useState<string | null>(null)
  const [updateOpen, setUpdateOpen] = useState(false)
  const updater = useAppUpdater(appVersion)

  const addLog = useCallback((tone: LogEntry['tone'], text: string) => {
    setLogs(current => [createLog(tone, text), ...current].slice(0, 100))
  }, [])

  const logBackupCleanup = useCallback((cleanup?: BackupCleanupResult) => {
    if (!cleanup) return
    if (cleanup.removedCount > 0) {
      addLog(
        'ok',
        `已自动整理 ${cleanup.removedCount} 个旧备份，释放 ${formatStorage(cleanup.reclaimedBytes)}，当前保留 ${cleanup.remainingCount} 个备份。`,
      )
    }
    for (const warning of cleanup.warnings) {
      addLog('warn', `备份整理提示：${warning}`)
    }
  }, [addLog])

  const refreshBackups = useCallback(async () => {
    if (!isTauriDesktop()) return null
    setBackupLoading(true)
    try {
      const summary = await call<BackupSummary>('list_backups')
      setBackups(summary)
      for (const warning of summary.warnings) {
        addLog('warn', `备份读取提示：${warning}`)
      }
      return summary
    } catch (error) {
      addLog('warn', `读取备份列表失败：${errorMessage(error)}`)
      return null
    } finally {
      setBackupLoading(false)
    }
  }, [addLog])

  useEffect(() => {
    localStorage.setItem(LOG_STORAGE_KEY, JSON.stringify(logs))
  }, [logs])

  useEffect(() => {
    if (!isTauriDesktop()) return
    void getVersion()
      .then(setAppVersion)
      .catch(error => addLog('warn', `读取应用版本失败：${errorMessage(error)}`))
  }, [addLog])

  useEffect(() => {
    if (!toast) return
    const timer = window.setTimeout(() => setToast(null), 2600)
    return () => window.clearTimeout(timer)
  }, [toast])

  const refreshDesktop = useCallback(async (initialize: boolean) => {
    setScanning(true)
    setScanError(null)
    try {
      const result = await call<DesktopRefresh>('refresh_desktop', {
        selectedSources: desktop?.selectedSources ?? [],
        targetProvider: desktop?.targetProvider ?? desktop?.scan.currentProvider ?? '',
        observedProvider: desktop?.scan.currentProvider ?? 'unknown',
        initialize,
      })
      setDesktop(result)
      setSelectedIds(current => {
        const available = new Set(result.localSessions.map(session => session.id))
        return new Set([...current].filter(id => available.has(id)))
      })
      addLog(
        'ok',
        `扫描完成：发现 ${result.localSessions.length} 个本地会话，已排除 ${result.scan.remoteExcludedSessions} 个明确远端会话。`,
      )
      logBackupCleanup(result.backupCleanup)
      return result
    } catch (error) {
      const message = errorMessage(error)
      setScanError(message)
      addLog('warn', `扫描失败：${message}`)
      throw error
    } finally {
      setScanning(false)
    }
  }, [addLog, desktop, logBackupCleanup])

  useEffect(() => {
    if (!isTauriDesktop()) {
      setScanning(false)
      return
    }
    void refreshDesktop(true).catch(() => undefined)
  }, [])

  useEffect(() => {
    if (!technicalOpen || backups) return
    void refreshBackups()
  }, [backups, refreshBackups, technicalOpen])

  const sessions = desktop?.localSessions ?? []
  const filteredSessions = useMemo(() => {
    return sessions.filter(session => sessionMatchesSearch(session, deferredQuery, searchScope))
  }, [deferredQuery, searchScope, sessions])
  const groups = useMemo(() => groupSessions(filteredSessions), [filteredSessions])
  const recoverableCount = useMemo(
    () => sessions.reduce((count, session) => count + Number(session.status === 'recoverable'), 0),
    [sessions],
  )
  const attentionCount = useMemo(
    () => sessions.reduce((count, session) => count + Number(session.status === 'needsConfirmation'), 0),
    [sessions],
  )
  const archivedCount = useMemo(
    () => sessions.reduce((count, session) => count + Number(session.status === 'archived'), 0),
    [sessions],
  )
  const selectedCount = selectedIds.size
  const recoveryBusy = ['previewing', 'closing', 'repairing', 'refreshing', 'rollingBack'].includes(recoveryPhase)
  const updateBusy = updater.status === 'checking' || updater.status === 'downloading' || updater.status === 'installing'

  const selectedThreadIds = useCallback((range: RecoveryRange) => (
    range === 'selected' ? [...selectedIds] : undefined
  ), [selectedIds])

  const previewRecovery = useCallback(async (
    range: RecoveryRange,
    sourceDesktop = desktop,
  ) => {
    if (!sourceDesktop) return null
    setRecoveryPhase('previewing')
    setActivePreview(null)
    setRecoveryError(null)
    setRecoveryLockConflict(false)
    try {
      const preview = await call<Preview>('preview_projection', {
        selectedSources: sourceDesktop.selectedSources,
        targetProvider: sourceDesktop.targetProvider,
        selectedThreadIds: selectedThreadIds(range),
      })
      setActivePreview(preview)
      setRecoveryPhase('idle')
      addLog(
        'info',
        `恢复检查完成：${preview.changedThreads} 个会话需要深度处理，其中 ${preview.rolloutUpdates} 个需要同步会话元数据。`,
      )
      return preview
    } catch (error) {
      const message = errorMessage(error)
      setRecoveryError(message)
      setRecoveryPhase('error')
      addLog('warn', `恢复检查失败：${message}`)
      return null
    }
  }, [addLog, desktop, selectedThreadIds])

  const openRecovery = () => {
    const range: RecoveryRange = selectedCount > 0 ? 'selected' : 'all'
    setRecoveryRange(range)
    setRecoveryMode('safe')
    setRepairResult(null)
    setRepairProgress(null)
    setRecoveryError(null)
    setRecoveryLockConflict(false)
    setRestartPath(null)
    setRecoveryOpen(true)
    void previewRecovery(range)
  }

  const changeRecoveryRange = (range: RecoveryRange) => {
    setRecoveryRange(range)
    void previewRecovery(range)
  }

  const closeBlockingProcesses = useCallback(async (processes: BlockingProcess[]) => {
    const groups = uniqueProcessGroups(processes)
    const protectedProcess = groups.find(process => !process.closeAllowed)
    if (protectedProcess) {
      throw new Error(`${protectedProcess.identity.name} 无法由工具安全关闭，请手动关闭后重试。`)
    }
    const restartable = groups.find(process => process.restartable && process.identity.path)
    if (restartable?.identity.path) setRestartPath(restartable.identity.path)

    for (const process of groups) {
      const result = await call<CloseResult>('close_blocking_process', {
        identity: process.identity,
        force: false,
      })
      addLog(result.exited ? 'ok' : 'warn', `${process.identity.name}：${result.message}`)
      if (!result.exited) {
        throw new Error(`${process.identity.name} 未能安全退出。请先保存工作并手动关闭该程序。`)
      }
    }
  }, [addLog])

  const performRecovery = useCallback(async (current: DesktopRefresh) => {
    setRepairProgress(null)
    setRecoveryPhase('previewing')
    const latestPreview = await call<Preview>('preview_projection', {
      selectedSources: current.selectedSources,
      targetProvider: current.targetProvider,
      selectedThreadIds: selectedThreadIds(recoveryRange),
    })
    setActivePreview(latestPreview)
    setRecoveryPhase('repairing')
    addLog('info', '开始在线备份并修复会话元数据与本地索引；Codex 可保持运行。')
    const onProgress = new Channel<RepairProgress>()
    onProgress.onmessage = progress => setRepairProgress(progress)
    const result = await call<RepairResult>('repair_indexes', {
      selectedSources: current.selectedSources,
      targetProvider: current.targetProvider,
      selectedThreadIds: selectedThreadIds(recoveryRange),
      dryRun: false,
      planToken: latestPreview.planToken,
      onProgress,
    })
    setRepairResult(result)
    setBackups(null)
    logBackupCleanup(result.backupCleanup)

    setRecoveryPhase('refreshing')
    try {
      await refreshDesktop(false)
    } catch (error) {
      addLog('warn', `后端修复已完成，但会话列表刷新失败：${errorMessage(error)}`)
    }
    setRecoveryPhase('success')
    if (result.verified) {
      setToast('会话可见性已恢复')
      addLog(
        'ok',
        `恢复完成并通过后端验证：处理 ${result.changedThreads} 个会话，其中 ${result.rolloutUpdates} 个已同步会话元数据。`,
      )
    } else {
      setToast('已恢复可安全处理的会话')
      addLog('warn', `在线恢复已提交；${result.skipped} 个记录因冲突或边界规则被安全跳过。`)
    }
  }, [addLog, logBackupCleanup, recoveryRange, refreshDesktop, selectedThreadIds])

  const startRecovery = useCallback(async () => {
    if (!desktop) return
    setRecoveryPhase('previewing')
    setRecoveryError(null)
    setRecoveryLockConflict(false)
    setRepairResult(null)
    setRepairProgress(null)
    try {
      const current = await refreshDesktop(false)
      await performRecovery(current)
    } catch (error) {
      const message = errorMessage(error)
      const lockConflict = isDatabaseBusy(error)
      setRecoveryError(message)
      setRecoveryLockConflict(lockConflict)
      setRecoveryPhase('error')
      if (lockConflict) {
        addLog('warn', '在线恢复遇到真实数据库写冲突；未关闭任何程序，可直接重试或使用安全关闭兜底。')
      }
      addLog('warn', `恢复未完成：${message}`)
    }
  }, [
    addLog,
    desktop,
    performRecovery,
    refreshDesktop,
  ])

  const closeProcessesAndRetry = useCallback(async () => {
    if (!desktop) return
    setRecoveryError(null)
    setRecoveryLockConflict(false)
    setRepairResult(null)
    setRepairProgress(null)
    try {
      setRecoveryPhase('closing')
      const current = await refreshDesktop(false)
      if (current.blockingProcesses.length > 0) {
        addLog('info', '用户确认使用兜底流程：正在安全关闭占用会话数据库的程序。')
        await closeBlockingProcesses(current.blockingProcesses)
      } else {
        addLog('info', '未发现仍占用会话数据库的程序，直接重试在线恢复。')
      }
      const ready = current.blockingProcesses.length > 0
        ? await refreshDesktop(false)
        : current
      await performRecovery(ready)
    } catch (error) {
      const message = errorMessage(error)
      setRecoveryError(message)
      setRecoveryLockConflict(isDatabaseBusy(error))
      setRecoveryPhase('error')
      addLog('warn', `兜底恢复未完成：${message}`)
    }
  }, [addLog, closeBlockingProcesses, desktop, performRecovery, refreshDesktop])

  const cleanupBackups = useCallback(async (includeLegacy: boolean) => {
    if (includeLegacy) {
      const confirmed = window.confirm(
        '旧版与损坏备份无法由当前版本恢复，删除后不可撤销。manifest 已损坏的备份可能无法识别原锁定状态；正常且可校验的锁定备份和正在使用的备份不会被删除。\n\n是否继续清理？',
      )
      if (!confirmed) return
    }
    setBackupLoading(true)
    try {
      const cleanup = await call<BackupCleanupResult>('cleanup_backups', { includeLegacy })
      logBackupCleanup(cleanup)
      if (cleanup.removedCount === 0 && cleanup.warnings.length === 0) {
        addLog('info', '备份已经符合保留策略，无需清理。')
      }
      await refreshBackups()
      setToast(cleanup.removedCount > 0 ? '旧备份已清理' : '备份无需清理')
    } catch (error) {
      const message = errorMessage(error)
      addLog('warn', `清理备份失败：${message}`)
      setToast('备份清理未完成')
    } finally {
      setBackupLoading(false)
    }
  }, [addLog, logBackupCleanup, refreshBackups])

  const retainBackup = useCallback(async (entry: BackupEntry) => {
    setBackupActionPath(entry.path)
    try {
      const summary = await call<BackupSummary>('set_backup_retained', {
        backupPath: entry.path,
        retained: !entry.pinned,
      })
      setBackups(summary)
      addLog(
        'ok',
        entry.pinned
          ? `备份已恢复自动管理：${entry.name}`
          : `备份已锁定保留：${entry.name}`,
      )
    } catch (error) {
      addLog('warn', `修改备份保留状态失败：${errorMessage(error)}`)
    } finally {
      setBackupActionPath(null)
    }
  }, [addLog])

  const openBackupFolder = useCallback(async () => {
    try {
      await call('open_backup_folder')
    } catch (error) {
      addLog('warn', `打开备份目录失败：${errorMessage(error)}`)
    }
  }, [addLog])

  const restoreBackup = useCallback(async (entry: BackupEntry) => {
    if (!entry.restorable || entry.protected) return
    setBackupActionPath(entry.path)
    try {
      const current = await refreshDesktop(false)
      if (current.scan.pendingOperation) {
        setToast('请先处理未完成的恢复')
        addLog('warn', '检测到尚未收尾的修复或恢复操作，请先完成安全回滚或刷新收尾，再选择历史备份。')
        return
      }
      if (current.blockingProcesses.length > 0) {
        setToast('历史回滚前请先关闭 Codex')
        addLog('warn', '恢复历史备份前请先保存工作并关闭正在占用会话数据库的 Codex 程序。')
        return
      }
      const time = new Date(entry.createdAt).toLocaleString('zh-CN', { hour12: false })
      const confirmed = window.confirm(
        `将恢复 ${time} 的会话索引快照${entry.provider ? `（Provider：${entry.provider}）` : ''}。执行前会自动备份当前状态。\n\n请确认已关闭 Codex，是否继续？`,
      )
      if (!confirmed) return
      setRecoveryPhase('rollingBack')
      await call<VerifyResult>('restore_backup', { backupPath: entry.path })
      await refreshDesktop(false)
      await refreshBackups()
      setRecoveryPhase('idle')
      setToast('历史备份已恢复')
      addLog('ok', `已恢复历史备份：${entry.name}。恢复前状态也已保留为安全快照。`)
    } catch (error) {
      const message = errorMessage(error)
      setRecoveryError(message)
      setRecoveryPhase('error')
      addLog('warn', `历史备份恢复失败：${message}`)
    } finally {
      setBackupActionPath(null)
    }
  }, [addLog, refreshBackups, refreshDesktop])

  const rollback = useCallback(async () => {
    const pendingRepair = desktop?.scan.pendingOperation?.command === 'repair'
    if (!pendingRepair && !desktop?.scan.lastBackup) return
    try {
      if (pendingRepair) {
        const confirmed = window.confirm(
          '安全回滚只撤销本次修复仍保持工具写入值的会话元数据与本地索引字段；Codex 后续新增或已经修改的数据会保留。\n\n可以保持 Codex 运行。是否继续？',
        )
        if (!confirmed) {
          addLog('info', '已取消安全回滚，当前数据保持不变。')
          return
        }
        setRecoveryPhase('rollingBack')
        await call<VerifyResult>('rollback_latest')
        await refreshDesktop(false)
        setBackups(null)
        setRecoveryOpen(false)
        setRecoveryPhase('idle')
        setToast('未完成修复已安全回滚')
        addLog('ok', '未完成修复已按逐行条件安全回滚，Codex 后续数据已保留。')
        return
      }

      const current = await refreshDesktop(false)
      if (current.blockingProcesses.length > 0) {
        const message = '离线回滚前请先手动关闭 Codex。在线修复不会自动关闭程序，也不会自动触发整库回滚。'
        setToast('离线回滚前请先关闭 Codex')
        addLog('warn', message)
        return
      }
      const confirmed = window.confirm(
        '离线回滚会恢复修复前的官方会话索引、rollout Provider 元数据与本地投影状态，修复后新产生的会话活动可能被撤销。\n\n请确认已关闭 Codex，并且确实需要回滚。',
      )
      if (!confirmed) {
        addLog('info', '已取消离线回滚，当前数据保持不变。')
        return
      }
      setRecoveryPhase('rollingBack')
      await call<VerifyResult>('rollback_latest')
      await refreshDesktop(false)
      setBackups(null)
      setRecoveryOpen(false)
      setRecoveryPhase('idle')
      setToast('已回滚最近一次恢复')
      addLog('ok', '最近一次修复备份已恢复。')
    } catch (error) {
      const message = errorMessage(error)
      setRecoveryError(message)
      setRecoveryPhase('error')
      addLog('warn', `回滚失败：${message}`)
    }
  }, [addLog, desktop?.scan.lastBackup, desktop?.scan.pendingOperation?.command, refreshDesktop])

  const reopenCodex = useCallback(async () => {
    if (!restartPath) {
      setRecoveryOpen(false)
      return
    }
    try {
      await call('reopen_codex', { executablePath: restartPath })
      addLog('ok', `已重新打开 Codex：${restartPath}`)
      setRecoveryOpen(false)
    } catch (error) {
      const message = errorMessage(error)
      setRecoveryError(message)
      setRecoveryPhase('error')
      addLog('warn', `重新打开 Codex 失败：${message}`)
    }
  }, [addLog, restartPath])

  const toggleSession = (id: string) => {
    setSelectedIds(current => {
      const next = new Set(current)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  const toggleGroupSelection = (group: SessionGroup) => {
    setSelectedIds(current => {
      const next = new Set(current)
      const allSelected = group.sessions.every(session => next.has(session.id))
      for (const session of group.sessions) {
        if (allSelected) next.delete(session.id)
        else next.add(session.id)
      }
      return next
    })
  }

  const toggleGroupOpen = (key: string) => {
    startTransition(() => {
      setExpandedGroups(current => {
        const next = new Set(current)
        if (next.has(key)) next.delete(key)
        else next.add(key)
        return next
      })
    })
  }

  const openProjectFolder = async (group: SessionGroup) => {
    try {
      await call<string>('open_project_folder', { path: group.path })
      setToast(`已打开项目文件夹：${group.name}`)
    } catch (error) {
      const message = errorMessage(error)
      setToast(`无法打开项目文件夹：${message}`)
      addLog('warn', `打开项目文件夹失败：${group.path}，${message}`)
    }
  }

  const copySessionId = async (session: LocalSession) => {
    try {
      await copyText(session.id)
      setToast('已复制完整会话 ID')
    } catch (error) {
      const message = errorMessage(error)
      setToast(`复制会话 ID 失败：${message}`)
      addLog('warn', `复制会话 ID 失败：${session.id}，${message}`)
    }
  }

  const revealRollout = async (session: LocalSession) => {
    if (!session.rolloutPath) return
    try {
      await call<string>('reveal_rollout_file', { path: session.rolloutPath })
      setToast('已在文件管理器中定位 rollout JSONL')
    } catch (error) {
      const message = errorMessage(error)
      setToast(`无法定位 rollout JSONL：${message}`)
      addLog('warn', `定位 rollout JSONL 失败：${session.rolloutPath}，${message}`)
    }
  }

  const openRepository = async () => {
    try {
      await openProjectRepository()
    } catch (error) {
      const message = errorMessage(error)
      setToast(`无法打开项目仓库：${message}`)
      addLog('warn', `打开项目仓库失败：${message}`)
    }
  }

  if (!isTauriDesktop()) {
    return (
      <main className="desktop-required">
        <span><ShieldCheck size={28} /></span>
        <h1>请打开桌面应用</h1>
        <p>Codex 会话恢复只在已安装的 EXE 桌面端运行。浏览器页面不会读取或修改本地会话。</p>
      </main>
    )
  }

  return (
    <div className="app-shell">
      <header className="app-header">
        <div className="brand-lockup">
          <span className="brand-mark"><Sparkles size={19} /></span>
          <span className="brand-copy">
            <span className="brand-title"><strong>Codex 会话恢复</strong><button type="button" onClick={() => setUpdateOpen(true)} disabled={recoveryBusy}>v{appVersion ?? '...'}</button></span>
            <small>本地会话可见性修复</small>
          </span>
        </div>
        <div className="header-actions">
          <span
            className={`runtime-status${scanError ? ' error' : ''}`}
            role="status"
            aria-live="polite"
            title={scanError ?? undefined}
          >
            {scanError ? <CircleAlert size={14} /> : <span className="status-dot" />}
            {scanError ? (desktop ? '刷新失败，显示上次结果' : '扫描异常') : scanning ? '正在扫描' : '本地运行中'}
          </span>
          <button className="icon-button" type="button" title="打开项目仓库" aria-label="打开项目仓库" onClick={() => void openRepository()}>
            <Github size={17} />
          </button>
          <button className="secondary-button compact-button update-button" type="button" onClick={() => setUpdateOpen(true)} disabled={recoveryBusy}>
            <Download size={16} />版本更新
          </button>
          <button className="icon-button" type="button" title="重新扫描" aria-label="重新扫描" onClick={() => void refreshDesktop(false).catch(() => undefined)} disabled={scanning}>
            <RefreshCw className={scanning ? 'spin' : ''} size={17} />
          </button>
          <button className="secondary-button compact-button" type="button" onClick={() => setTechnicalOpen(true)}>
            <Activity size={16} />技术详情
          </button>
        </div>
      </header>

      <main className="workspace">
        <section className="workspace-heading">
          <div>
            <span className="workspace-kicker">LOCAL CODEX SESSIONS</span>
            <h1>本地会话</h1>
            <p>
              {desktop
                ? scanError
                  ? `本次刷新失败，继续显示上次成功加载的 ${sessions.length} 个会话。`
                  : `${sessions.length} 个本地会话，包含普通、内部和归档记录；明确远端会话暂不进入恢复范围。`
                : '正在读取本地会话。'}
            </p>
          </div>
          <button className="primary-button repair-button" type="button" onClick={openRecovery} disabled={scanning || updateBusy || !desktop || sessions.length === 0}>
            <ShieldCheck size={18} />{selectedCount ? `恢复选中的 ${selectedCount} 个会话` : '恢复全部会话'}
          </button>
        </section>

        <section className="session-workspace">
          <div className="session-toolbar">
            <div className="search-field">
              <Search size={17} />
              <select
                aria-label="搜索范围"
                value={searchScope}
                onChange={event => setSearchScope(event.target.value as SearchScope)}
              >
                <option value="title">会话标题</option>
                <option value="project">项目名称</option>
              </select>
              <span className="search-divider" aria-hidden="true" />
              <input
                aria-label={searchPlaceholders[searchScope]}
                value={query}
                onChange={event => setQuery(event.target.value)}
                placeholder={searchPlaceholders[searchScope]}
              />
              {query ? <button type="button" aria-label="清除搜索" onClick={() => setQuery('')}><X size={15} /></button> : null}
            </div>
            <div className="toolbar-summary">
              {selectedCount ? <button className="selection-chip" type="button" onClick={() => setSelectedIds(new Set())}>{selectedCount} 个已选择 <X size={13} /></button> : null}
              <span><CheckCircle2 size={15} />{sessions.length - recoverableCount - attentionCount - archivedCount} 个当前可见</span>
              <span className="recoverable-summary"><FolderSearch size={15} />{recoverableCount} 个可恢复</span>
            </div>
          </div>

          <div className="list-column-head" aria-hidden="true">
            <span>项目与会话</span><span>原始来源</span><span>状态</span><span>最近更新</span>
          </div>

          {scanning && !desktop ? (
            <div className="loading-state"><LoaderCircle className="spin" size={25} /><strong>正在扫描本地会话</strong><span>仅读取索引和会话元数据。</span></div>
          ) : scanError && !desktop ? (
            <div className="error-state"><CircleAlert size={25} /><strong>无法读取本地会话</strong><span>{scanError}</span><button className="secondary-button" type="button" onClick={() => void refreshDesktop(true).catch(() => undefined)}>重新扫描</button></div>
          ) : (
            <SessionExplorer
              groups={groups}
              expandedGroups={expandedGroups}
              selectedIds={selectedIds}
              forceOpen={Boolean(deferredQuery)}
              onToggleGroupOpen={toggleGroupOpen}
              onToggleGroupSelection={toggleGroupSelection}
              onToggleSession={toggleSession}
              onOpenProject={group => void openProjectFolder(group)}
              onCopySessionId={session => void copySessionId(session)}
              onRevealRollout={session => void revealRollout(session)}
            />
          )}

          <footer className="workspace-footer" aria-label="本地会话环境状态">
            <span className="footer-source" title={desktop?.scan.codexHome}>
              <span className="status-dot" />
              <span className="footer-label">数据目录</span>
              <code>{desktop?.scan.codexHome ?? '等待扫描'}</code>
            </span>
            <span className="footer-meta">
              <span className="footer-provider" title={desktop?.scan.currentProvider}>
                <SlidersHorizontal size={15} />
                <span className="footer-label">Provider</span>
                <code>{desktop?.scan.currentProvider ?? '-'}</code>
              </span>
              <span className="footer-divider" aria-hidden="true" />
              {desktop?.scan.lastBackup
                ? <span className="footer-backup"><ShieldCheck size={15} />回滚快照已就绪</span>
                : <span className="footer-backup"><ShieldCheck size={15} />写入前自动备份</span>}
            </span>
          </footer>
        </section>
      </main>

      <RecoveryDialog
        open={recoveryOpen}
        targetProvider={desktop?.targetProvider ?? '-'}
        totalCount={sessions.length}
        selectedCount={selectedCount}
        range={recoveryRange}
        mode={recoveryMode}
        phase={recoveryPhase}
        progress={repairProgress}
        preview={activePreview}
        result={repairResult}
        blockerCount={desktop?.blockingProcesses.length ?? 0}
        error={recoveryError}
        lockConflict={recoveryLockConflict}
        advancedOpen={advancedOpen}
        canReopen={Boolean(restartPath)}
        onClose={() => { if (!['previewing', 'closing', 'repairing', 'refreshing', 'rollingBack'].includes(recoveryPhase)) setRecoveryOpen(false) }}
        onRangeChange={changeRecoveryRange}
        onModeChange={setRecoveryMode}
        onAdvancedToggle={() => setAdvancedOpen(value => !value)}
        onStart={() => void startRecovery()}
        onRetry={() => void startRecovery()}
        onCloseAndRetry={() => void closeProcessesAndRetry()}
        onReopen={() => void reopenCodex()}
        onRollback={() => void rollback()}
      />

      <AppUpdateDialog
        open={updateOpen}
        currentVersion={appVersion ?? '未知'}
        status={updater.status}
        latestVersion={updater.latestVersion}
        releaseNotes={updater.releaseNotes}
        progress={updater.progress}
        error={updater.error}
        onClose={() => setUpdateOpen(false)}
        onCheck={() => void updater.checkForUpdates()}
        onInstall={() => void updater.installUpdate()}
      />

      <TechnicalDrawer
        open={technicalOpen}
        scan={desktop?.scan ?? null}
        localSessionCount={sessions.length}
        processes={desktop?.blockingProcesses ?? []}
        logs={logs}
        backups={backups}
        backupLoading={backupLoading}
        backupActionPath={backupActionPath}
        busy={recoveryPhase === 'rollingBack' || backupLoading || Boolean(backupActionPath)}
        onClose={() => setTechnicalOpen(false)}
        onRollback={() => void rollback()}
        onRefreshBackups={() => void refreshBackups()}
        onOpenBackupFolder={() => void openBackupFolder()}
        onCleanupBackups={includeLegacy => void cleanupBackups(includeLegacy)}
        onRetainBackup={entry => void retainBackup(entry)}
        onRestoreBackup={entry => void restoreBackup(entry)}
        onClearLogs={() => setLogs([])}
      />

      {toast ? <div className="toast" role="status" aria-live="polite"><CheckCircle2 size={17} />{toast}</div> : null}
    </div>
  )
}
