import {
  CheckCircle2,
  CircleAlert,
  Database,
  FileClock,
  RotateCcw,
  Terminal,
  Trash2,
  X,
} from 'lucide-react'
import type { BlockingProcess, LogEntry, Scan } from '../app-types'

type TechnicalDrawerProps = {
  open: boolean
  scan: Scan | null
  localSessionCount: number
  processes: BlockingProcess[]
  logs: LogEntry[]
  busy: boolean
  onClose: () => void
  onRollback: () => void
  onClearLogs: () => void
}

export function TechnicalDrawer({
  open,
  scan,
  localSessionCount,
  processes,
  logs,
  busy,
  onClose,
  onRollback,
  onClearLogs,
}: TechnicalDrawerProps) {
  if (!open) return null

  const pendingRepair = scan?.pendingOperation?.command === 'repair'
  const pendingPhase = scan?.pendingOperation?.phase
  const pendingMessage = pendingPhase === 'verificationFailed'
    ? '本次修复写入后验证未通过。逐行安全回滚记录已保留，可以保持 Codex 运行并执行安全回滚。'
    : pendingPhase === 'compensating'
      ? '安全回滚尚未完全收尾。外部修改已保留，请刷新后查看冲突详情。'
      : pendingRepair
        ? '检测到未收尾修复。刷新会核对现场并安全收尾，请保留 pending、WAL 与 SHM 文件。'
        : scan?.pendingOperation
          ? `检测到未收尾的 ${scan.pendingOperation.command} 操作。请保留现场并刷新。`
          : null
  const rollbackLabel = pendingRepair ? '安全回滚未完成修复' : '离线回滚最近一次'
  const rollbackPath = pendingRepair ? scan?.pendingOperation?.backupPath : scan?.lastBackup
  const stateSource = scan?.sources.find(source => source.name === 'threads')
  const catalogSource = scan?.sources.find(source => source.name === 'local_thread_catalog')

  return (
    <div className="drawer-backdrop" role="presentation" onMouseDown={onClose}>
      <aside
        className="technical-drawer"
        role="dialog"
        aria-modal="true"
        aria-labelledby="technical-drawer-title"
        onMouseDown={event => event.stopPropagation()}
      >
        <header className="drawer-header">
          <div><span>LOCAL DIAGNOSTICS</span><h2 id="technical-drawer-title">技术详情与日志</h2></div>
          <button className="icon-button" type="button" aria-label="关闭技术详情" onClick={onClose}><X size={18} /></button>
        </header>

        <div className="drawer-scroll">
          <section className="detail-section">
            <h3><Database size={16} />本地数据</h3>
            <dl className="detail-list">
              <div><dt>CODEX_HOME</dt><dd title={scan?.codexHome}>{scan?.codexHome ?? '未发现'}</dd></div>
              <div><dt>当前 Provider</dt><dd>{scan?.currentProvider ?? '-'}</dd></div>
              <div><dt>本地会话</dt><dd>{localSessionCount}</dd></div>
              <div><dt>远端会话</dt><dd>已排除 {scan?.remoteExcludedSessions ?? 0}</dd></div>
              <div><dt>官方会话索引</dt><dd>{stateSource?.readable ? 'state_5.sqlite 可读' : '不可用'}</dd></div>
              <div><dt>辅助目录</dt><dd>{catalogSource?.readable ? '可读，仅用于诊断' : '不可用，不阻塞修复'}</dd></div>
            </dl>
          </section>

          <section className="detail-section">
            <h3><CircleAlert size={16} />运行状态</h3>
            {processes.length === 0 ? (
              <p className="detail-ok"><CheckCircle2 size={15} />当前未检测到 Codex 相关数据库进程。</p>
            ) : (
              <div className="process-detail-list">
                {processes.map(process => (
                  <div key={`${process.identity.pid}-${process.identity.startedAt ?? ''}`}>
                    <span><strong>{process.identity.name}</strong><small>PID {process.identity.pid}</small></span>
                    <em>{process.closeAllowed ? '在线修复可保持运行' : '写冲突时需手动处理'}</em>
                  </div>
                ))}
              </div>
            )}
            {pendingMessage ? (
              <p className="detail-warning">{pendingMessage}</p>
            ) : null}
          </section>

          <section className="detail-section">
            <div className="section-title-row">
              <h3><FileClock size={16} />备份与回滚</h3>
              <button className="text-button danger" type="button" onClick={onRollback} disabled={(!scan?.lastBackup && !pendingRepair) || busy}>
                <RotateCcw size={14} />{rollbackLabel}
              </button>
            </div>
            <p className="path-value" title={rollbackPath}>{rollbackPath ?? '尚未创建修复备份'}</p>
          </section>

          <section className="detail-section log-section">
            <div className="section-title-row">
              <h3><Terminal size={16} />执行日志</h3>
              <button className="icon-button compact" type="button" title="清空执行日志" aria-label="清空执行日志" onClick={onClearLogs}><Trash2 size={16} /></button>
            </div>
            <div className="technical-logs" role="log" aria-live="polite" aria-relevant="additions text">
              {logs.length ? logs.map(log => (
                <div className={`technical-log ${log.tone}`} key={log.id}>
                  <time>{log.time}</time><span>{log.text}</span>
                </div>
              )) : <p className="empty-log">暂无执行日志。</p>}
            </div>
          </section>
        </div>
      </aside>
    </div>
  )
}
