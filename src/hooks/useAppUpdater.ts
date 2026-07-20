import { useCallback, useRef, useState } from 'react'
import type { Update } from '@tauri-apps/plugin-updater'

export type AppUpdateStatus =
  | 'idle'
  | 'checking'
  | 'upToDate'
  | 'available'
  | 'downloading'
  | 'installing'
  | 'error'

export type AppUpdateProgress = {
  downloaded: number
  total: number | null
  percent: number | null
}

export type AppUpdateLogger = (tone: 'info' | 'ok' | 'warn', message: string) => void

export type CheckForUpdatesOptions = {
  /** Quiet startup / background checks: skip routine info logs. */
  silent?: boolean
}

const EMPTY_PROGRESS: AppUpdateProgress = {
  downloaded: 0,
  total: null,
  percent: null,
}

/** Primary endpoint from tauri.conf.json — used only for diagnostics copy. */
export const UPDATE_MANIFEST_URL =
  'https://github.com/XVibeCoding/codex-session-hub/releases/latest/download/latest.json'

const UPDATE_CHECK_TIMEOUT_MS = 20_000
const UPDATE_CHECK_MAX_ATTEMPTS = 3
const UPDATE_CHECK_RETRY_BASE_MS = 1_000

function normalizeVersion(version: string) {
  return version.trim().replace(/^v/i, '')
}

function wait(milliseconds: number) {
  return new Promise<void>(resolve => window.setTimeout(resolve, milliseconds))
}

function formatElapsed(startedAt: number) {
  return `${((Date.now() - startedAt) / 1000).toFixed(1)} 秒`
}

function rawUpdateError(error: unknown) {
  return String(error).replace(/^Error:\s*/i, '').trim()
}

function diagnosticUpdateError(error: unknown) {
  const message = rawUpdateError(error)
    .replace(/[A-Z]:\\Users\\[^\\\s]+/gi, '<user-home>')
    .replace(/\s+/g, ' ')
  return (message || 'unknown updater error').slice(0, 320)
}

function readableUpdateError(error: unknown) {
  const message = rawUpdateError(error)
  if (/404|not found|valid release json|release json|latest\.json/i.test(message)) {
    return '未获取到在线更新清单 latest.json。请确认 GitHub 上最新正式 Release 已发布（非草稿）且包含 latest.json，或到项目 Releases 页手动下载安装包。'
  }
  if (/json|deserialize|parse|invalid response|invalid data/i.test(message)) {
    return '在线更新清单格式异常。请稍后重试，或到项目 Releases 页手动下载安装包。'
  }
  if (/timed?\s*out|network|connection|dns|request|fetch/i.test(message)) {
    return '暂时无法连接更新服务器，请检查网络后重试。'
  }
  if (/signature|verify|public key/i.test(message)) {
    return '更新包签名校验未通过，已停止安装。请到项目仓库确认正式版本。'
  }
  return message || '检查更新失败，请稍后重试。'
}

function isTransferBusy(status: AppUpdateStatus) {
  return status === 'downloading' || status === 'installing'
}

export function useAppUpdater(onLog?: AppUpdateLogger) {
  const updateRef = useRef<Update | null>(null)
  const checkInFlightRef = useRef<Promise<void> | null>(null)
  const statusRef = useRef<AppUpdateStatus>('idle')
  const [status, setStatus] = useState<AppUpdateStatus>('idle')
  const [latestVersion, setLatestVersion] = useState<string | null>(null)
  const [releaseNotes, setReleaseNotes] = useState<string | null>(null)
  const [progress, setProgress] = useState<AppUpdateProgress>(EMPTY_PROGRESS)
  const [error, setError] = useState<string | null>(null)
  const [lastCheckedAt, setLastCheckedAt] = useState<number | null>(null)

  const setStatusTracked = useCallback((next: AppUpdateStatus) => {
    statusRef.current = next
    setStatus(next)
  }, [])

  const checkForUpdates = useCallback((options: CheckForUpdatesOptions = {}) => {
    const silent = Boolean(options.silent)
    if (checkInFlightRef.current) return checkInFlightRef.current
    if (isTransferBusy(statusRef.current)) return Promise.resolve()

    const operation = (async () => {
      const startedAt = Date.now()
      setStatusTracked('checking')
      setError(null)
      // Keep previous latestVersion while checking so the header badge does not flicker off.
      setReleaseNotes(null)
      setProgress(EMPTY_PROGRESS)
      if (!silent) {
        onLog?.('info', '开始检查应用更新，正在连接 GitHub Releases…')
      }

      try {
        if (updateRef.current) {
          await updateRef.current.close().catch(() => undefined)
          updateRef.current = null
        }

        const { check } = await import('@tauri-apps/plugin-updater')
        let update: Update | null = null
        let lastError: unknown

        for (let attempt = 0; attempt < UPDATE_CHECK_MAX_ATTEMPTS; attempt += 1) {
          const attemptStartedAt = Date.now()
          try {
            update = await check({ timeout: UPDATE_CHECK_TIMEOUT_MS })
            lastError = undefined
            break
          } catch (caught) {
            lastError = caught
            const remaining = UPDATE_CHECK_MAX_ATTEMPTS - attempt - 1
            if (remaining > 0) {
              const delay = UPDATE_CHECK_RETRY_BASE_MS * (attempt + 1)
              if (!silent) {
                onLog?.(
                  'info',
                  `第 ${attempt + 1} 次更新检查未完成（${formatElapsed(attemptStartedAt)}），${(delay / 1000).toFixed(1)} 秒后自动重试。诊断：${diagnosticUpdateError(caught)}`,
                )
              }
              await wait(delay)
            }
          }
        }

        if (lastError) throw lastError

        setLastCheckedAt(Date.now())

        if (!update) {
          // Plugin null means “no higher installable version for this build”,
          // not “GitHub has no releases”. Keep latestVersion cleared so UI does
          // not imply a pending package.
          setLatestVersion(null)
          setReleaseNotes(null)
          setStatusTracked('upToDate')
          if (!silent) {
            onLog?.(
              'ok',
              `应用更新检查完成（${formatElapsed(startedAt)}）：当前没有可安装的更高版本。若网页 Releases 已有新包但仍显示最新，请确认最新正式版（非草稿）是否附带 latest.json。`,
            )
          }
          return
        }

        updateRef.current = update
        const version = normalizeVersion(update.version)
        setLatestVersion(version)
        setReleaseNotes(update.body?.trim() || null)
        setStatusTracked('available')
        onLog?.('ok', `应用更新检查完成（${formatElapsed(startedAt)}）：发现新版本 v${version}。`)
      } catch (caught) {
        const message = readableUpdateError(caught)
        setError(message)
        setStatusTracked('error')
        setLastCheckedAt(Date.now())
        onLog?.(
          'warn',
          `应用更新检查失败（${formatElapsed(startedAt)}）：${message} 诊断：${diagnosticUpdateError(caught)}`,
        )
      }
    })()

    checkInFlightRef.current = operation
    void operation.finally(() => {
      if (checkInFlightRef.current === operation) checkInFlightRef.current = null
    })
    return operation
  }, [onLog, setStatusTracked])

  const installUpdate = useCallback(async () => {
    const update = updateRef.current
    if (!update || isTransferBusy(statusRef.current)) return
    if (import.meta.env.DEV) {
      setError('当前是开发调试版本，不会覆盖本机已安装的生产应用。请用正式安装包执行自动更新。')
      setStatusTracked('error')
      return
    }

    let downloaded = 0
    let total: number | null = null
    setError(null)
    setProgress(EMPTY_PROGRESS)
    setStatusTracked('downloading')
    onLog?.('info', `开始下载应用更新 v${normalizeVersion(update.version)}…`)
    try {
      await update.download(event => {
        if (event.event === 'Started') {
          downloaded = 0
          total = event.data.contentLength ?? null
          setProgress({ downloaded, total, percent: total ? 0 : null })
          return
        }
        if (event.event === 'Progress') {
          downloaded += event.data.chunkLength
          const percent = total ? Math.min(100, Math.round((downloaded / total) * 100)) : null
          setProgress({ downloaded, total, percent })
          return
        }
        setProgress(current => ({ ...current, percent: 100 }))
      }, { timeout: 5 * 60_000 })

      setProgress(current => ({ ...current, percent: 100 }))
      setStatusTracked('installing')
      onLog?.('ok', `应用更新 v${normalizeVersion(update.version)} 已下载并通过签名校验，即将调用系统安装程序。`)
      await update.install()

      const { relaunch } = await import('@tauri-apps/plugin-process')
      await relaunch()
    } catch (caught) {
      const message = readableUpdateError(caught)
      setError(message)
      setStatusTracked('error')
      onLog?.('warn', `应用更新安装失败：${message}`)
    }
  }, [onLog, setStatusTracked])

  return {
    status,
    latestVersion,
    releaseNotes,
    progress,
    error,
    lastCheckedAt,
    checkForUpdates,
    installUpdate,
  }
}
