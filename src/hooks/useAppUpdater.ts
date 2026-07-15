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

const EMPTY_PROGRESS: AppUpdateProgress = {
  downloaded: 0,
  total: null,
  percent: null,
}

const UPDATE_CHECK_TIMEOUT_MS = 10_000
const UPDATE_CHECK_RETRY_DELAY_MS = 800

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
    return '未读取到在线更新清单 latest.json。请稍后重试，或到项目 Releases 页面手动下载安装包。'
  }
  if (/json|deserialize|parse|invalid response|invalid data/i.test(message)) {
    return '在线更新清单格式异常，请稍后重试，或到项目 Releases 页面手动下载安装包。'
  }
  if (/timed?\s*out|network|connection|dns|request|fetch/i.test(message)) {
    return '暂时无法连接更新服务器，请检查网络后重试。'
  }
  if (/signature|verify|public key/i.test(message)) {
    return '更新包签名校验未通过，已停止安装。请从项目仓库确认正式版本。'
  }
  return message || '检查更新失败，请稍后重试。'
}

export function useAppUpdater(onLog?: AppUpdateLogger) {
  const updateRef = useRef<Update | null>(null)
  const checkInFlightRef = useRef<Promise<void> | null>(null)
  const [status, setStatus] = useState<AppUpdateStatus>('idle')
  const [latestVersion, setLatestVersion] = useState<string | null>(null)
  const [releaseNotes, setReleaseNotes] = useState<string | null>(null)
  const [progress, setProgress] = useState<AppUpdateProgress>(EMPTY_PROGRESS)
  const [error, setError] = useState<string | null>(null)

  const checkForUpdates = useCallback(() => {
    if (checkInFlightRef.current) return checkInFlightRef.current
    if (status === 'downloading' || status === 'installing') return Promise.resolve()

    const operation = (async () => {
      const startedAt = Date.now()
      setStatus('checking')
      setError(null)
      setLatestVersion(null)
      setReleaseNotes(null)
      setProgress(EMPTY_PROGRESS)
      onLog?.('info', '开始检查应用更新：正在连接 GitHub Releases。')

      try {
        if (updateRef.current) {
          await updateRef.current.close().catch(() => undefined)
          updateRef.current = null
        }

        const { check } = await import('@tauri-apps/plugin-updater')
        let update: Update | null = null
        let lastError: unknown

        for (let attempt = 0; attempt < 2; attempt += 1) {
          const attemptStartedAt = Date.now()
          try {
            update = await check({ timeout: UPDATE_CHECK_TIMEOUT_MS })
            lastError = undefined
            break
          } catch (caught) {
            lastError = caught
            if (attempt === 0) {
              onLog?.(
                'info',
                `首次更新检查未完成（${formatElapsed(attemptStartedAt)}），稍后自动重试一次。诊断：${diagnosticUpdateError(caught)}`,
              )
              await wait(UPDATE_CHECK_RETRY_DELAY_MS)
            }
          }
        }

        if (lastError) throw lastError
        if (!update) {
          setStatus('upToDate')
          onLog?.('ok', `应用更新检查完成（${formatElapsed(startedAt)}）：当前已是最新版。`)
          return
        }

        updateRef.current = update
        const version = normalizeVersion(update.version)
        setLatestVersion(version)
        setReleaseNotes(update.body?.trim() || null)
        setStatus('available')
        onLog?.('ok', `应用更新检查完成（${formatElapsed(startedAt)}）：发现新版本 v${version}。`)
      } catch (caught) {
        const message = readableUpdateError(caught)
        setError(message)
        setStatus('error')
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
  }, [onLog, status])

  const installUpdate = useCallback(async () => {
    const update = updateRef.current
    if (!update || status === 'downloading' || status === 'installing') return
    if (import.meta.env.DEV) {
      setError('当前是开发调试版本，不会覆盖正在运行的程序。请从正式安装版执行自动更新。')
      setStatus('error')
      return
    }

    let downloaded = 0
    let total: number | null = null
    setError(null)
    setProgress(EMPTY_PROGRESS)
    setStatus('downloading')
    onLog?.('info', `开始下载应用更新 v${normalizeVersion(update.version)}。`)
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
      setStatus('installing')
      onLog?.('ok', `应用更新 v${normalizeVersion(update.version)} 下载完成且签名校验通过，正在启动系统安装程序。`)
      await update.install()

      const { relaunch } = await import('@tauri-apps/plugin-process')
      await relaunch()
    } catch (caught) {
      const message = readableUpdateError(caught)
      setError(message)
      setStatus('error')
      onLog?.('warn', `应用更新安装失败：${message}`)
    }
  }, [onLog, status])

  return {
    status,
    latestVersion,
    releaseNotes,
    progress,
    error,
    checkForUpdates,
    installUpdate,
  }
}
