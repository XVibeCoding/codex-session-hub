import { useEffect } from 'react'
import {
  CheckCircle2,
  CircleAlert,
  Download,
  ExternalLink,
  Github,
  LoaderCircle,
  RefreshCw,
  X,
} from 'lucide-react'
import type { AppUpdateProgress, AppUpdateStatus } from '../hooks/useAppUpdater'

const REPOSITORY_URL = 'https://github.com/XVibeCoding/codex-provider-hub'
const RELEASES_URL = `${REPOSITORY_URL}/releases`

type AppUpdateDialogProps = {
  open: boolean
  currentVersion: string
  status: AppUpdateStatus
  latestVersion: string | null
  releaseNotes: string | null
  progress: AppUpdateProgress
  error: string | null
  onClose: () => void
  onCheck: () => void
  onInstall: () => void
}

function formatBytes(value: number) {
  if (value < 1024) return `${value} B`
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`
  return `${(value / 1024 / 1024).toFixed(1)} MB`
}

async function openExternal(url: string) {
  const { openUrl } = await import('@tauri-apps/plugin-opener')
  await openUrl(url)
}

export function openProjectRepository() {
  return openExternal(REPOSITORY_URL)
}

export function AppUpdateDialog({
  open,
  currentVersion,
  status,
  latestVersion,
  releaseNotes,
  progress,
  error,
  onClose,
  onCheck,
  onInstall,
}: AppUpdateDialogProps) {
  useEffect(() => {
    if (open && status === 'idle') onCheck()
  }, [onCheck, open, status])

  if (!open) return null
  const transferBusy = status === 'downloading' || status === 'installing'
  const percentLabel = progress.percent === null ? null : `${progress.percent}%`

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={transferBusy ? undefined : onClose}>
      <section
        className="update-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="update-title"
        onMouseDown={event => event.stopPropagation()}
      >
        <header className="dialog-header update-dialog-header">
          <div>
            <span className="dialog-kicker">APPLICATION UPDATE</span>
            <h2 id="update-title">应用更新</h2>
            <p>当前版本 <strong>v{currentVersion}</strong>，更新包由项目签名验证后安装。</p>
          </div>
          <button className="icon-button" type="button" aria-label="关闭" onClick={onClose} disabled={transferBusy}>
            <X size={18} />
          </button>
        </header>

        <div className="update-dialog-body" aria-live="polite">
          {status === 'checking' ? (
            <div className="update-state">
              <LoaderCircle className="spin" size={30} />
              <h3>正在检查新版本</h3>
              <p>正在连接 GitHub Releases，连接失败时会自动重试一次。</p>
            </div>
          ) : status === 'available' ? (
            <div className="update-state available">
              <span className="update-state-icon"><Download size={24} /></span>
              <h3>发现新版本 v{latestVersion}</h3>
              <p>下载后会启动系统安装程序，应用可能自动关闭并重新打开。</p>
              {releaseNotes ? <div className="release-notes">{releaseNotes}</div> : null}
              <button className="primary-button" type="button" onClick={onInstall}>
                <Download size={17} />下载并安装
              </button>
            </div>
          ) : status === 'downloading' || status === 'installing' ? (
            <div className="update-state">
              <LoaderCircle className="spin" size={30} />
              <h3>{status === 'downloading' ? '正在下载更新' : '正在启动安装程序'}</h3>
              <p>{status === 'downloading' ? '请保持网络连接，不要关闭应用。' : '安装程序接管后，当前窗口会自动关闭。'}</p>
              <div className="update-progress" aria-label="更新下载进度">
                <div className="progress-track" aria-hidden="true">
                  <span className={progress.percent === null ? 'indeterminate' : ''} style={progress.percent === null ? undefined : { width: `${progress.percent}%` }} />
                </div>
                <div className="progress-meta">
                  <span>{status === 'installing' ? '准备安装' : progress.total ? `${formatBytes(progress.downloaded)} / ${formatBytes(progress.total)}` : formatBytes(progress.downloaded)}</span>
                  <strong>{status === 'installing' ? '100%' : percentLabel ?? '下载中'}</strong>
                </div>
              </div>
            </div>
          ) : status === 'upToDate' ? (
            <div className="update-state">
              <span className="update-state-icon success"><CheckCircle2 size={25} /></span>
              <h3>当前已是最新版本</h3>
              <p>已成功检查 GitHub Releases。已安装 v{currentVersion}，当前没有更高版本。</p>
              <div className="update-actions">
                <button className="secondary-button" type="button" onClick={onCheck}><RefreshCw size={16} />重新检查</button>
                <button className="secondary-button" type="button" onClick={() => void openExternal(RELEASES_URL)}><ExternalLink size={16} />查看 Releases</button>
              </div>
            </div>
          ) : status === 'error' ? (
            <div className="update-state">
              <span className="update-state-icon error"><CircleAlert size={25} /></span>
              <h3>暂时无法完成更新</h3>
              <p>{error}</p>
              <div className="update-actions">
                <button className="secondary-button" type="button" onClick={onCheck}><RefreshCw size={16} />重新检查</button>
                <button className="secondary-button" type="button" onClick={() => void openExternal(RELEASES_URL)}><ExternalLink size={16} />查看 Releases</button>
              </div>
            </div>
          ) : null}
        </div>

        <footer className="repository-footer">
          <Github size={17} />
          <span><small>项目仓库</small><strong>{REPOSITORY_URL.replace('https://', '')}</strong></span>
          <button className="icon-button compact" type="button" title="打开项目仓库" aria-label="打开项目仓库" onClick={() => void openExternal(REPOSITORY_URL)}>
            <ExternalLink size={16} />
          </button>
        </footer>
      </section>
    </div>
  )
}
