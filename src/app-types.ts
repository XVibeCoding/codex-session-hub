export type Provider = {
  id: string
  name: string
  sourceSessions: number
  currentlyVisible: number
}

export type SourceSummary = {
  name: string
  path: string
  records: number
  readable: boolean
  note: string
}

export type Scan = {
  codexHome: string
  currentProvider: string
  providers: Provider[]
  recoverableSessions: number
  remoteExcludedSessions: number
  sqlite: number
  lock: string
  needsAdmin: boolean
  lastBackup?: string
  pendingOperation?: {
    command: string
    backupPath: string
    createdAt: string
    phase?: 'prepared' | 'compensating' | 'committed' | 'verificationFailed'
  }
  sources: SourceSummary[]
}

export type LocalSession = {
  id: string
  title: string
  cwd: string
  rolloutPath: string
  projectName: string
  provider: string
  originProvider: string
  updatedAt: number
  archived: boolean
  internal: boolean
  visibility: 'visible' | 'hidden'
  status: 'visible' | 'recoverable' | 'archived' | 'needsConfirmation'
}

export type Preview = {
  plan: {
    considered: number
    pending: number
    matrix: { aligned: number }
  }
  planToken: string
  changedThreads: number
  rolloutUpdates: number
  reconcilePending: number
  reconcileConflicts: number
  workspaceHintUpdates: number
  workspaceConflicts: number
  skipped: number
}

export type ProcessIdentity = {
  pid: number
  name: string
  path?: string
  startedAt?: string
  parentPid?: number
  sessionId?: number
  verified: boolean
  isCurrent: boolean
  isAncestor: boolean
}

export type BlockingProcess = {
  identity: ProcessIdentity
  applicationType: string
  restartable: boolean
  applicationRootPid?: number
  closeAllowed: boolean
  closeReason: string
}

export type DesktopRefresh = {
  scan: Scan
  preview: Preview
  localSessions: LocalSession[]
  blockingProcesses: BlockingProcess[]
  selectedSources: string[]
  targetProvider: string
  backupCleanup?: BackupCleanupResult
}

export type BackupKind = 'automatic' | 'manual' | 'restoreSafety'

export type BackupEntry = {
  name: string
  path: string
  createdAt: string
  sizeBytes: number
  provider: string
  kind: BackupKind
  pinned: boolean
  protected: boolean
  protectionReason?: string
  restorable: boolean
  status: 'restorable' | 'legacy' | 'corrupt' | 'incomplete'
  manifestVersion?: number
}

export type BackupSummary = {
  entries: BackupEntry[]
  restorableCount: number
  automaticCount: number
  pinnedCount: number
  legacyCount: number
  incompleteCount: number
  totalBytes: number
  legacyBytes: number
  automaticLimit: number
  minimumAutomatic: number
  capacityLimitBytes: number
  overLimit: boolean
  warnings: string[]
}

export type BackupCleanupResult = {
  removedCount: number
  removedLegacyCount: number
  reclaimedBytes: number
  remainingCount: number
  remainingBytes: number
  warnings: string[]
}

export type RepairResult = {
  changedThreads: number
  restoredThreads: number
  stateUpdates: number
  rolloutUpdates: number
  catalogUpdates: number
  catalogInserts: number
  workspaceHintUpdates: number
  skipped: number
  verified: boolean
  backupPath?: string
  backupCleanup?: BackupCleanupResult
}

export type VerifyResult = {
  ok: boolean
  checked: number
  remaining: number
  skipped: number
}

export type CloseResult = {
  pid: number
  mode: string
  requested: boolean
  exited: boolean
  message: string
}

export type RecoveryRange = 'all' | 'selected'
export type RecoveryMode = 'safe' | 'full'
export type RecoveryPhase =
  | 'idle'
  | 'previewing'
  | 'closing'
  | 'repairing'
  | 'refreshing'
  | 'success'
  | 'error'
  | 'rollingBack'

export type RepairProgressStage =
  | 'planning'
  | 'acquiringOperationLock'
  | 'planValidated'
  | 'acquiringWriteFence'
  | 'backup'
  | 'sqliteStaging'
  | 'metadataSync'
  | 'commit'
  | 'verification'
  | 'completed'

export type RepairProgress = {
  stage: RepairProgressStage
  percent: number
  message: string
  completed?: number
  total?: number
}

export type LogEntry = {
  id: string
  time: string
  tone: 'ok' | 'warn' | 'info'
  text: string
}

export type SessionGroup = {
  key: string
  name: string
  path: string
  sessions: LocalSession[]
  latest: number
}
