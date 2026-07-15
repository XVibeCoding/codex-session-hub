import {
  ArrowRight,
  Check,
  ChevronDown,
  ChevronUp,
  CircleAlert,
  DatabaseBackup,
  Info,
  LoaderCircle,
  RotateCcw,
  ShieldCheck,
  X,
} from 'lucide-react'
import type {
  Preview,
  RecoveryMode,
  RecoveryPhase,
  RecoveryRange,
  RepairResult,
} from '../app-types'

type RecoveryDialogProps = {
  open: boolean
  targetProvider: string
  totalCount: number
  selectedCount: number
  range: RecoveryRange
  mode: RecoveryMode
  phase: RecoveryPhase
  preview: Preview | null
  result: RepairResult | null
  blockerCount: number
  error: string | null
  lockConflict: boolean
  advancedOpen: boolean
  canReopen: boolean
  onClose: () => void
  onRangeChange: (range: RecoveryRange) => void
  onModeChange: (mode: RecoveryMode) => void
  onAdvancedToggle: () => void
  onStart: () => void
  onRetry: () => void
  onCloseAndRetry: () => void
  onReopen: () => void
  onRollback: () => void
}

const runningPhases: RecoveryPhase[] = ['previewing', 'closing', 'repairing', 'verifying', 'rollingBack']

function ProgressSteps({ phase }: { phase: RecoveryPhase }) {
  const current = phase === 'repairing' ? 1 : phase === 'verifying' ? 2 : 0
  const steps = ['在线创建备份', '对齐官方索引与会话元数据', '验证会话可见性']
  return (
    <div className="recovery-progress">
      {steps.map((step, index) => (
        <div className={`progress-step${index < current ? ' done' : ''}${index === current ? ' active' : ''}`} key={step}>
          <span>{index < current ? <Check size={14} /> : index + 1}</span>
          <strong>{step}</strong>
        </div>
      ))}
    </div>
  )
}

export function RecoveryDialog({
  open,
  targetProvider,
  totalCount,
  selectedCount,
  range,
  mode,
  phase,
  preview,
  result,
  blockerCount,
  error,
  lockConflict,
  advancedOpen,
  canReopen,
  onClose,
  onRangeChange,
  onModeChange,
  onAdvancedToggle,
  onStart,
  onRetry,
  onCloseAndRetry,
  onReopen,
  onRollback,
}: RecoveryDialogProps) {
  if (!open) return null
  const busy = runningPhases.includes(phase)
  const scopeCount = range === 'selected' ? selectedCount : totalCount

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={busy ? undefined : onClose}>
      <section
        className="recovery-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="recovery-title"
        onMouseDown={event => event.stopPropagation()}
      >
        <header className="dialog-header">
          <div>
            <span className="dialog-kicker">CODEX SESSION REPAIR</span>
            <h2 id="recovery-title">恢复会话可见性</h2>
            <p>恢复到当前 Provider：<code>{targetProvider}</code>。账号、密钥和模型配置保持不变。</p>
          </div>
          <button className="icon-button" type="button" aria-label="关闭" onClick={onClose} disabled={busy}>
            <X size={18} />
          </button>
        </header>

        {phase === 'success' && result ? (
          <div className="recovery-success">
            <span className="success-icon"><Check size={28} /></span>
            <h3>{result.verified ? '会话可见性已恢复' : '已完成可安全恢复的会话'}</h3>
            <p>
              已处理 {result.changedThreads} 个会话并完成后端验证。
              {result.rolloutUpdates ? ` 其中 ${result.rolloutUpdates} 个会话已同步本地元数据。` : ''}
              {!result.verified && result.skipped ? ` ${result.skipped} 个记录因冲突或安全边界被跳过。` : ''}
              {result.backupPath ? ' 修复前快照已保留。' : ' 当前数据已经对齐，无需额外写入。'}
            </p>
            <div className="success-actions">
              <button className="secondary-button" type="button" onClick={onClose}>返回会话列表</button>
              <button className="primary-button" type="button" onClick={onReopen}>
                {canReopen ? '重新打开 Codex 查看' : '完成'}<ArrowRight size={17} />
              </button>
            </div>
            {result.backupPath ? (
              <button className="text-button danger" type="button" onClick={onRollback}>
                <RotateCcw size={14} />离线回滚本次恢复
              </button>
            ) : null}
          </div>
        ) : busy ? (
          <div className="recovery-running">
            <LoaderCircle className="spin" size={28} />
            <h3>{phase === 'previewing' ? '正在检查恢复范围' : phase === 'closing' ? '正在安全关闭占用程序' : phase === 'repairing' ? '正在在线备份并修复' : phase === 'rollingBack' ? '正在执行离线回滚' : '正在验证结果'}</h3>
            <p>
              {phase === 'closing'
                ? '仅在在线写入发生真实冲突后执行；关闭完成会自动重试恢复。'
                : phase === 'rollingBack'
                  ? '整库回滚需要离线执行，不会自动关闭或重启其他程序。'
                  : 'Codex 可以保持打开。请保持此窗口运行，修复前会创建在线备份。'}
            </p>
            {!['closing', 'rollingBack'].includes(phase) ? <ProgressSteps phase={phase} /> : null}
          </div>
        ) : (
          <div className="dialog-body">
            <fieldset className="option-section">
              <legend>会话范围</legend>
              <div className="choice-grid">
                <label className={`choice-card${range === 'all' ? ' selected' : ''}`}>
                  <input type="radio" name="range" checked={range === 'all'} onChange={() => onRangeChange('all')} />
                  <span><strong>全部本地会话</strong><small>检查当前列表中的 {totalCount} 个会话</small></span>
                  <em>{totalCount}</em>
                </label>
                <label className={`choice-card${range === 'selected' ? ' selected' : ''}${selectedCount === 0 ? ' disabled' : ''}`}>
                  <input type="radio" name="range" checked={range === 'selected'} disabled={selectedCount === 0} onChange={() => onRangeChange('selected')} />
                  <span><strong>仅所选会话</strong><small>{selectedCount ? `只处理已选择的 ${selectedCount} 个会话` : '先在列表中选择会话'}</small></span>
                  <em>{selectedCount}</em>
                </label>
              </div>
            </fieldset>

            <fieldset className="option-section">
              <legend>修复方式</legend>
              <label className="method-card selected">
                <input type="radio" name="mode" checked={mode === 'safe'} onChange={() => onModeChange('safe')} />
                <span className="method-icon"><ShieldCheck size={19} /></span>
                <span><strong>在线安全恢复 <b>推荐</b></strong><small>Codex 无需提前关闭；自动备份并对齐官方会话索引与 rollout 元数据，不改账号、密钥或模型配置。</small></span>
                <Check size={17} />
              </label>
              <button className="advanced-toggle" type="button" onClick={onAdvancedToggle}>
                高级恢复选项{advancedOpen ? <ChevronUp size={15} /> : <ChevronDown size={15} />}
              </button>
              {advancedOpen ? (
                <label className="method-card disabled" aria-disabled="true">
                  <input type="radio" name="mode" checked={mode === 'full'} disabled onChange={() => onModeChange('full')} />
                  <span className="method-icon"><DatabaseBackup size={19} /></span>
                  <span><strong>离线完整恢复</strong><small>仅在在线恢复后仍不可见时使用，需要先关闭 Codex；当前版本先保持关闭。</small></span>
                </label>
              ) : null}
            </fieldset>

            <div className="preview-summary">
              {error ? (
                <div className="inline-error"><CircleAlert size={17} /><span><strong>恢复未完成</strong>{error}</span></div>
              ) : preview ? (
                <>
                  <div><strong>{preview.changedThreads}</strong><span>预计需要恢复</span></div>
                  <div><strong>{Math.max(preview.plan.considered - preview.changedThreads, 0)}</strong><span>已经对齐</span></div>
                  <div><strong>{preview.workspaceConflicts + preview.reconcileConflicts}</strong><span>需要确认</span></div>
                </>
              ) : (
                <span className="preview-loading"><LoaderCircle className="spin" size={15} />正在检查 {scopeCount} 个会话</span>
              )}
            </div>

            {lockConflict ? (
              <div className="process-notice lock-conflict-notice">
                <CircleAlert size={16} />
                <span>在线恢复遇到真实写入冲突。可先直接重试；若持续发生，再安全关闭占用程序后重试。</span>
              </div>
            ) : blockerCount > 0 ? (
              <div className="process-notice informational">
                <Info size={16} />
                <span>检测到 {blockerCount} 个 Codex 相关进程。普通打开状态通常不影响在线恢复，无需提前关闭。</span>
              </div>
            ) : null}
          </div>
        )}

        {phase !== 'success' && !busy ? (
          <footer className="dialog-footer">
            <button className="secondary-button" type="button" onClick={onClose}>取消</button>
            {lockConflict ? (
              <>
                <button className="secondary-button" type="button" onClick={onRetry} disabled={scopeCount === 0}>直接重试</button>
                <button className="primary-button" type="button" onClick={onCloseAndRetry} disabled={scopeCount === 0}>
                  关闭占用程序并重试<ArrowRight size={17} />
                </button>
              </>
            ) : (
              <button className="primary-button" type="button" onClick={error ? onRetry : onStart} disabled={scopeCount === 0 || (!error && !preview)}>
                {error ? '重新检查并恢复' : `在线恢复 ${scopeCount} 个会话`}<ArrowRight size={17} />
              </button>
            )}
          </footer>
        ) : null}
      </section>
    </div>
  )
}
