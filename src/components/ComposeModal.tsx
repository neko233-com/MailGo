import { motion } from 'motion/react'
import { useCallback, useEffect, useRef, useState } from 'react'
import { Icon } from './Icon'
import { RecipientInput } from './RecipientInput'
import { RichTextEditor } from './RichTextEditor'
import { ScheduleSendControl } from './ScheduleSendControl'
import { TooltipButton } from './TooltipButton'
import { buildComposeThreadHeaders, type ComposeMode } from '../compose-thread'
import { mapWithConcurrency } from '../lib/asyncPool'
import { invoke } from '../lib/ipc'
import { appendSignatureToComposeHtml, plainTextToComposeHtml, sanitizeComposeHtml } from '../richText'
import { appendAccountSignature, normalizeAccountSignature } from '../signature'
import type { MailMessage, NativeAttachmentChunkResponse, NativeAttachmentStartResponse, NativeAttachmentUploadChunkResponse, NativeAttachmentUploadStartResponse, NativeDraft, NativeDraftAttachment, NativeSendResponse } from '../types'

const ACCOUNT_IPC_CONCURRENCY = 4
const DEFAULT_UNDO_SEND_SECONDS = 10


function escapeHtml(input: string) {
  return input
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

function bytesToBase64(bytes: Uint8Array) {
  let binary = ''
  const blockSize = 0x8000
  for (let offset = 0; offset < bytes.length; offset += blockSize) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + blockSize))
  }
  return btoa(binary)
}

type ComposeAttachmentStatus = 'saving' | 'saved' | 'failed' | 'removing'
type ComposeAttachmentItem = {
  localId: string
  persistedId?: string
  fileName: string
  contentType: string
  size: number
  file?: File
  status: ComposeAttachmentStatus
}
type ComposeInlineImage = ComposeAttachmentItem & { contentId: string; previewUrl?: string }

function composeSubject(mode: ComposeMode, subject: string) {
  const trimmed = subject.trim() || '(无主题)'
  if (mode === 'reply' || mode === 'reply-all') return /^(re\s*:\s*)/i.test(trimmed) ? trimmed : `Re: ${trimmed}`
  if (mode === 'forward') return /^(fwd\s*:\s*)/i.test(trimmed) ? trimmed : `Fwd: ${trimmed}`
  return trimmed
}

function uniqueRecipients(values: string[], ownEmail?: string) {
  const seen = new Set<string>()
  return values.filter((value) => {
    const normalized = value.trim().toLowerCase()
    if (!normalized || normalized === ownEmail?.trim().toLowerCase() || seen.has(normalized)) return false
    seen.add(normalized)
    return true
  })
}

function quoteMail(source: MailMessage, mode: ComposeMode) {
  const body = source.body.join('\n\n').slice(0, 20_000)
  if (mode === 'forward') {
    return `\n\n---------- 转发邮件 ----------\n发件人：${source.senderName} <${source.from}>\n主题：${source.subject}\n日期：${source.timestamp}\n\n${body}`
  }
  return `\n\n---------- 原始邮件 ----------\n${source.senderName} <${source.from}> 在 ${source.timestamp} 写道：\n${body.split(/\r?\n/).map((line) => `> ${line}`).join('\n')}`
}

function composeSeed(mode: ComposeMode, source: MailMessage | undefined, ownEmail?: string) {
  if (!source || mode === 'new') return { to: '', cc: '', subject: '', body: '' }
  const replyAllCc = uniqueRecipients([...(source.to ?? []), ...(source.cc ?? [])], ownEmail)
    .filter((recipient) => recipient.toLowerCase() !== source.from.trim().toLowerCase())
  return {
    to: source.from,
    cc: mode === 'reply-all' ? replyAllCc.join(', ') : '',
    subject: composeSubject(mode, source.subject),
    body: quoteMail(source, mode),
  }
}

function createInlineContentId() {
  const randomPart = typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
    ? crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(36).slice(2)}`
  return `mailgo-inline-${randomPart.replace(/[^a-zA-Z0-9-]/g, '')}`
}

function createComposeAttachmentId() {
  const randomPart = typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
    ? crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(36).slice(2)}`
  return `compose-attachment-${randomPart.replace(/[^a-zA-Z0-9-]/g, '')}`
}

function composeHtmlBody(body: string, inlineImages: ComposeInlineImage[], richBody?: string, signature = '') {
  const textBlock = richBody?.trim()
    ? sanitizeComposeHtml(richBody)
    : plainTextToComposeHtml(body)
  const signedBody = appendSignatureToComposeHtml(textBlock, signature)
  const imageBlocks = inlineImages.map(({ fileName, contentId }) => (
    `<p><img src="cid:${escapeHtml(contentId)}" alt="${escapeHtml(fileName)}"></p>`
  )).join('')
  return sanitizeComposeHtml(signedBody + imageBlocks)
}

export function ComposeModal({ mode, source, accountId, senderEmail, signature = '', draftId: openDraftId, onClose, onSent, onError, onDraftChanged, onDraftRemoved }: { mode: ComposeMode; source?: MailMessage; accountId?: string; senderEmail?: string; signature?: string; draftId?: string; onClose: () => void; onSent: (result: NativeSendResponse) => void; onError: (message: string) => void; onDraftChanged?: (draft: NativeDraft) => void; onDraftRemoved?: (draftId: string) => void }) {
  const [to, setTo] = useState('')
  const [cc, setCc] = useState('')
  const [bcc, setBcc] = useState('')
  const [showCopyFields, setShowCopyFields] = useState(false)
  const [subject, setSubject] = useState('')
  const [body, setBody] = useState('')
  const [richBody, setRichBody] = useState('')
  const [htmlMode, setHtmlMode] = useState(false)
  const [inReplyTo, setInReplyTo] = useState<string | undefined>()
  const [references, setReferences] = useState<string[]>([])
  const [draftId, setDraftId] = useState<string | undefined>()
  const [draftStatus, setDraftStatus] = useState('')
  const [draftReady, setDraftReady] = useState(false)
  const [attachments, setAttachments] = useState<ComposeAttachmentItem[]>([])
  const [inlineImages, setInlineImages] = useState<ComposeInlineImage[]>([])
  const [isSending, setSending] = useState(false)
  const [uploadingName, setUploadingName] = useState('')
  const fileInputRef = useRef<HTMLInputElement>(null)
  const imageInputRef = useRef<HTMLInputElement>(null)
  const attachmentsRef = useRef<ComposeAttachmentItem[]>([])
  const inlineImagesRef = useRef<ComposeInlineImage[]>([])
  const draftIdRef = useRef<string | undefined>(undefined)
  const pendingDraftSaveRef = useRef<Promise<NativeDraft | undefined>>(Promise.resolve(undefined))
  const pendingAttachmentJobsRef = useRef(new Set<Promise<void>>())
  const attachmentPersistenceQueueRef = useRef<Promise<void>>(Promise.resolve())
  const isNativeRuntime = Boolean(window.ipc?.postMessage)
  const maxAttachmentBytes = 25 * 1024 * 1024
  const maxTotalAttachmentBytes = 50 * 1024 * 1024
  const accountSignature = normalizeAccountSignature(signature)

  useEffect(() => {
    attachmentsRef.current = attachments
  }, [attachments])

  useEffect(() => {
    inlineImagesRef.current = inlineImages
  }, [inlineImages])

  useEffect(() => {
    draftIdRef.current = draftId
  }, [draftId])

  useEffect(() => () => {
    inlineImagesRef.current.forEach((image) => { if (image.previewUrl) URL.revokeObjectURL(image.previewUrl) })
  }, [])

  const clearInlineImages = () => {
    setInlineImages((current) => {
      current.forEach((image) => { if (image.previewUrl) URL.revokeObjectURL(image.previewUrl) })
      inlineImagesRef.current = []
      return []
    })
  }

  const saveDraftSnapshot = useCallback((force = false) => {
    if (!isNativeRuntime || !accountId || !draftReady || isSending) return Promise.resolve(undefined)
    if (!force && !draftIdRef.current && ![to, cc, bcc, subject, body].some((value) => value.trim())) return Promise.resolve(undefined)
    const save = async () => {
      setDraftStatus('正在保存草稿…')
      const draft = await invoke<NativeDraft>('drafts.save', {
        ...(draftIdRef.current ? { id: draftIdRef.current } : {}),
        accountId,
        to,
        cc,
        bcc,
        subject,
        body,
        htmlMode,
        ...(htmlMode && richBody.trim() ? { htmlBody: richBody } : {}),
        ...(inReplyTo ? { inReplyTo } : {}),
        ...(references.length ? { references } : {}),
      }, 30_000)
      draftIdRef.current = draft.id
      setDraftId(draft.id)
      setDraftStatus('草稿已自动保存')
      onDraftChanged?.(draft)
      return draft
    }
    const request = pendingDraftSaveRef.current.catch(() => undefined).then(save)
    pendingDraftSaveRef.current = request
    return request
  }, [accountId, bcc, body, cc, draftReady, htmlMode, inReplyTo, isNativeRuntime, isSending, onDraftChanged, references, richBody, subject, to])

  const waitForAttachmentJobs = useCallback(async () => {
    while (pendingAttachmentJobsRef.current.size > 0) {
      await Promise.allSettled(Array.from(pendingAttachmentJobsRef.current))
    }
  }, [])

  const trackAttachmentJob = useCallback((job: Promise<void>) => {
    pendingAttachmentJobsRef.current.add(job)
    void job.finally(() => pendingAttachmentJobsRef.current.delete(job)).catch(() => undefined)
  }, [])

  const loadDraftInlinePreview = useCallback(async (savedDraftId: string, attachment: NativeDraftAttachment) => {
    if (!accountId) return undefined
    let downloadId: string | undefined
    try {
      const start = await invoke<NativeAttachmentStartResponse>('drafts.attachment.start', {
        accountId,
        draftId: savedDraftId,
        attachmentId: attachment.id,
      }, 60_000)
      downloadId = start.downloadId
      let offset = 0
      let total = 0
      const parts: Uint8Array[] = []
      while (true) {
        const chunk: NativeAttachmentChunkResponse = await invoke<NativeAttachmentChunkResponse>('mail.attachment.chunk', { downloadId, offset }, 60_000)
        if (chunk.downloadId !== downloadId || chunk.offset !== offset || chunk.nextOffset < offset || chunk.nextOffset > start.size || (!chunk.done && chunk.nextOffset === offset)) {
          throw new Error('草稿图片传输响应无效')
        }
        const bytes = Uint8Array.from(atob(chunk.dataBase64), (character) => character.charCodeAt(0))
        if (chunk.nextOffset - offset !== bytes.length) throw new Error('草稿图片传输大小校验失败')
        parts.push(bytes)
        total += bytes.length
        if (total > start.size) throw new Error('草稿图片超过声明大小')
        offset = chunk.nextOffset
        if (chunk.done) break
      }
      if (total !== start.size) throw new Error('草稿图片传输不完整')
      const binary = new Uint8Array(total)
      let writeOffset = 0
      for (const part of parts) {
        binary.set(part, writeOffset)
        writeOffset += part.length
      }
      return URL.createObjectURL(new Blob([binary], { type: start.contentType }))
    } catch (error) {
      if (downloadId) void invoke('mail.attachment.cancel', { downloadId }).catch(() => undefined)
      throw error
    }
  }, [accountId])

  useEffect(() => {
    let cancelled = false
    const seed = composeSeed(mode, source, senderEmail)
    const threadHeaders = buildComposeThreadHeaders(mode, source)
    setDraftReady(false)
    setDraftStatus('')
    draftIdRef.current = openDraftId
    setDraftId(openDraftId)
    setTo(seed.to)
    setCc(seed.cc)
    setBcc('')
    setShowCopyFields(Boolean(seed.cc))
    setSubject(seed.subject)
    setBody(seed.body)
    setRichBody('')
    setHtmlMode(false)
    setInReplyTo(threadHeaders.inReplyTo)
    setReferences(threadHeaders.references)
    attachmentsRef.current = []
    setAttachments([])
    clearInlineImages()
    if (!isNativeRuntime || !accountId || (!openDraftId && mode !== 'new')) {
      setDraftReady(true)
      return () => { cancelled = true }
    }
    void invoke<NativeDraft[]>('drafts.list', { accountId }, 30_000).then((drafts) => {
      const draft = openDraftId ? drafts.find((item) => item.id === openDraftId) : drafts[0]
      if (cancelled || !draft) return
      draftIdRef.current = draft.id
      setDraftId(draft.id)
      setTo(draft.to)
      setCc(draft.cc)
      setBcc(draft.bcc)
      setShowCopyFields(Boolean(draft.cc || draft.bcc))
      setSubject(draft.subject)
      setBody(draft.body)
      setRichBody(draft.htmlBody || (draft.htmlMode ? plainTextToComposeHtml(draft.body) : ''))
      setHtmlMode(draft.htmlMode)
      setInReplyTo(draft.inReplyTo)
      setReferences(draft.references)
      const restoredAttachments: ComposeAttachmentItem[] = []
      const restoredImages: ComposeInlineImage[] = []
      for (const attachment of draft.attachments ?? []) {
        const item = {
          localId: `restored-${attachment.id}`,
          persistedId: attachment.id,
          fileName: attachment.fileName,
          contentType: attachment.contentType,
          size: attachment.size,
          status: 'saved' as const,
        }
        if (attachment.contentId) restoredImages.push({ ...item, contentId: attachment.contentId })
        else restoredAttachments.push(item)
      }
      attachmentsRef.current = restoredAttachments
      inlineImagesRef.current = restoredImages
      setAttachments(restoredAttachments)
      setInlineImages(restoredImages)
      if (restoredImages.length) setHtmlMode(true)
      setDraftStatus(openDraftId ? '已恢复草稿' : '已恢复最近草稿')
      onDraftChanged?.(draft)
      void (async () => {
        for (const image of restoredImages) {
          if (cancelled || !image.persistedId) return
          const metadata = draft.attachments.find((attachment) => attachment.id === image.persistedId)
          if (!metadata) continue
          try {
            const previewUrl = await loadDraftInlinePreview(draft.id, metadata)
            if (!previewUrl) continue
            if (cancelled) {
              URL.revokeObjectURL(previewUrl)
              return
            }
            setInlineImages((current) => {
              const next = current.map((item) => item.persistedId === image.persistedId ? { ...item, previewUrl } : item)
              inlineImagesRef.current = next
              return next
            })
          } catch {
            if (!cancelled) setDraftStatus('草稿已恢复，部分图片预览暂不可用')
          }
        }
      })()
    }).catch(() => undefined).finally(() => {
      if (!cancelled) setDraftReady(true)
    })
    return () => { cancelled = true }
  }, [accountId, isNativeRuntime, loadDraftInlinePreview, mode, onDraftChanged, openDraftId, senderEmail, source])

  useEffect(() => {
    if (!isNativeRuntime || !accountId || !draftReady || isSending) return
    if (!draftIdRef.current && ![to, cc, bcc, subject, body].some((value) => value.trim())) return
    const timer = window.setTimeout(() => {
      void saveDraftSnapshot().catch(() => setDraftStatus('草稿保存失败，将在下次输入时重试'))
    }, 700)
    return () => window.clearTimeout(timer)
  }, [accountId, bcc, body, cc, draftReady, htmlMode, inReplyTo, isNativeRuntime, isSending, references, richBody, saveDraftSnapshot, subject, to])

  const addFiles = (event: React.ChangeEvent<HTMLInputElement>) => {
    const incoming = Array.from(event.target.files ?? [])
    event.target.value = ''
    let total = attachments.reduce((sum, attachment) => sum + attachment.size, 0)
      + inlineImages.reduce((sum, image) => sum + image.size, 0)
    const accepted: ComposeAttachmentItem[] = []
    for (const file of incoming) {
      if (attachments.length + inlineImages.length + accepted.length >= 10) {
        onError('单封邮件最多添加 10 个附件')
        break
      }
      if (file.size > maxAttachmentBytes) {
        onError(file.name + ' 超过单个附件 25 MB 限制')
        continue
      }
      if (total + file.size > maxTotalAttachmentBytes) {
        onError('附件总大小不能超过 50 MB')
        break
      }
      accepted.push({
        localId: createComposeAttachmentId(),
        fileName: file.name,
        contentType: file.type || 'application/octet-stream',
        size: file.size,
        file,
        status: isNativeRuntime ? 'saving' : 'saved',
      })
      total += file.size
    }
    if (accepted.length) {
      setAttachments((current) => {
        const next = [...current, ...accepted]
        attachmentsRef.current = next
        return next
      })
      if (isNativeRuntime) {
        const job = attachmentPersistenceQueueRef.current.catch(() => undefined).then(() => persistAttachmentItems(accepted))
        attachmentPersistenceQueueRef.current = job
        trackAttachmentJob(job)
      }
    }
  }

  const addInlineImages = (event: React.ChangeEvent<HTMLInputElement>) => {
    const incoming = Array.from(event.target.files ?? [])
    event.target.value = ''
    let total = attachments.reduce((sum, attachment) => sum + attachment.size, 0)
      + inlineImages.reduce((sum, image) => sum + image.size, 0)
    const accepted: ComposeInlineImage[] = []
    for (const file of incoming) {
      if (!file.type.toLowerCase().startsWith('image/')) {
        onError(`${file.name} 不是受支持的图片格式`)
        continue
      }
      if (attachments.length + inlineImages.length + accepted.length >= 10) {
        onError('单封邮件最多添加 10 个附件或内嵌图片')
        break
      }
      if (file.size > maxAttachmentBytes) {
        onError(file.name + ' 超过单个图片 25 MB 限制')
        continue
      }
      if (total + file.size > maxTotalAttachmentBytes) {
        onError('附件和内嵌图片总大小不能超过 50 MB')
        break
      }
      accepted.push({
        localId: createComposeAttachmentId(),
        fileName: file.name,
        contentType: file.type || 'application/octet-stream',
        size: file.size,
        file,
        contentId: createInlineContentId(),
        previewUrl: URL.createObjectURL(file),
        status: isNativeRuntime ? 'saving' : 'saved',
      })
      total += file.size
    }
    if (accepted.length) {
      setInlineImages((current) => {
        const next = [...current, ...accepted]
        inlineImagesRef.current = next
        return next
      })
      setHtmlMode(true)
      if (isNativeRuntime) {
        const job = attachmentPersistenceQueueRef.current.catch(() => undefined).then(() => persistAttachmentItems(accepted))
        attachmentPersistenceQueueRef.current = job
        trackAttachmentJob(job)
      }
    }
  }

  const uploadAttachment = async (file: File, contentId?: string) => {
    const start = await invoke<NativeAttachmentUploadStartResponse>('mail.attachment.upload.start', {
      fileName: file.name,
      contentType: file.type || 'application/octet-stream',
      size: file.size,
      ...(contentId ? { contentId } : {}),
    }, 60_000)
    if (start.done) return start.uploadId
    const chunkSize = Math.min(Math.max(1, start.chunkSize), 192 * 1024)
    let offset = 0
    try {
      while (offset < file.size) {
        const nextOffset = Math.min(file.size, offset + chunkSize)
        const bytes = new Uint8Array(await file.slice(offset, nextOffset).arrayBuffer())
        const result = await invoke<NativeAttachmentUploadChunkResponse>('mail.attachment.upload.chunk', {
          uploadId: start.uploadId,
          offset,
          dataBase64: bytesToBase64(bytes),
        }, 60_000)
        if (result.uploadId !== start.uploadId || result.offset !== offset || result.nextOffset !== nextOffset || result.nextOffset <= offset || result.nextOffset > file.size) {
          throw new Error('附件上传响应无效')
        }
        offset = result.nextOffset
        setUploadingName(file.name + ' ' + Math.round((offset / file.size) * 100) + '%')
        if (result.done !== (offset === file.size)) throw new Error('附件上传完成状态无效')
      }
      return start.uploadId
    } catch (error) {
      void invoke('mail.attachment.upload.cancel', { uploadId: start.uploadId }).catch(() => undefined)
      throw error
    }
  }

  const persistAttachmentItems = async (items: Array<ComposeAttachmentItem | ComposeInlineImage>) => {
    let draft: NativeDraft | undefined
    try {
      draft = await saveDraftSnapshot(true)
      if (!draft) throw new Error('无法创建本地草稿')
    } catch {
      const failedIds = new Set(items.map((item) => item.localId))
      setAttachments((current) => {
        const next = current.map((entry) => failedIds.has(entry.localId) ? { ...entry, status: 'failed' as const } : entry)
        attachmentsRef.current = next
        return next
      })
      setInlineImages((current) => {
        const next = current.map((entry) => failedIds.has(entry.localId) ? { ...entry, status: 'failed' as const } : entry)
        inlineImagesRef.current = next
        return next
      })
      setDraftStatus('附件无法写入本地草稿；关闭前请移除，或直接发送')
      onError('无法创建本地草稿，附件尚未安全保存')
      return
    }
    let cursor = 0
    const worker = async () => {
      while (cursor < items.length) {
        const item = items[cursor++]
        if (!item.file) continue
        let uploadId: string | undefined
        const expectedContentId = 'contentId' in item ? item.contentId : undefined
        const metadataMatches = (attachment: NativeDraftAttachment) => (
          attachment.id === item.localId
          && attachment.fileName === item.fileName
          && attachment.contentType === item.contentType
          && attachment.contentId === expectedContentId
          && attachment.size === item.size
        )
        const markSaved = (savedDraft: NativeDraft, attachment: NativeDraftAttachment) => {
          const update = <T extends ComposeAttachmentItem>(current: T[]) => current.map((entry) => (
            entry.localId === item.localId
              ? { ...entry, file: undefined, persistedId: attachment.id, status: 'saved' as const }
              : entry
          ))
          if ('contentId' in item) {
            setInlineImages((current) => {
              const next = update(current)
              inlineImagesRef.current = next
              return next
            })
          } else {
            setAttachments((current) => {
              const next = update(current)
              attachmentsRef.current = next
              return next
            })
          }
          setDraftStatus('附件已加密保存到草稿')
          onDraftChanged?.(savedDraft)
        }
        try {
          setUploadingName('正在加密保存 ' + item.fileName)
          uploadId = await uploadAttachment(item.file, expectedContentId)
          const result = await invoke<{ draft: NativeDraft; attachment: NativeDraftAttachment }>('drafts.attachment.commit', {
            accountId,
            draftId: draft.id,
            attachmentId: item.localId,
            uploadId,
          }, 60_000)
          uploadId = undefined
          if (!metadataMatches(result.attachment)) {
            throw new Error('草稿附件保存响应无效')
          }
          markSaved(result.draft, result.attachment)
        } catch {
          if (uploadId) void invoke('mail.attachment.upload.cancel', { uploadId }).catch(() => undefined)
          try {
            const savedDrafts = await invoke<NativeDraft[]>('drafts.list', { accountId }, 30_000)
            const recoveredDraft = savedDrafts.find((candidate) => candidate.id === draft.id)
            const recoveredAttachment = recoveredDraft?.attachments.find((attachment) => attachment.id === item.localId)
            if (recoveredDraft && recoveredAttachment && metadataMatches(recoveredAttachment)) {
              markSaved(recoveredDraft, recoveredAttachment)
              continue
            }
          } catch {
            // The original failure is reflected as a recoverable local-only attachment below.
          }
          const markFailed = <T extends ComposeAttachmentItem>(current: T[]) => current.map((entry) => (
            entry.localId === item.localId ? { ...entry, status: 'failed' as const } : entry
          ))
          if ('contentId' in item) {
            setInlineImages((current) => {
              const next = markFailed(current)
              inlineImagesRef.current = next
              return next
            })
          } else {
            setAttachments((current) => {
              const next = markFailed(current)
              attachmentsRef.current = next
              return next
            })
          }
          setDraftStatus('部分附件保存失败；发送前会重试，关闭窗口前请先处理')
          onError(`${item.fileName} 未能保存到本地草稿`)
        }
      }
    }
    await Promise.all(Array.from({ length: Math.min(3, items.length) }, () => worker()))
    try {
      const latestDrafts = await invoke<NativeDraft[]>('drafts.list', { accountId }, 30_000)
      const latestDraft = latestDrafts.find((candidate) => candidate.id === draft.id)
      if (latestDraft) onDraftChanged?.(latestDraft)
    } catch {
      // Per-item commit responses already updated the UI; a later draft refresh will reconcile it.
    }
    const hasFailure = [...attachmentsRef.current, ...inlineImagesRef.current]
      .some((attachment) => attachment.status === 'failed')
    setDraftStatus(hasFailure ? '部分附件保存失败；发送前会重试，关闭窗口前请先处理' : '附件已加密保存到草稿')
    setUploadingName('')
  }

  const removeComposeAttachment = async (item: ComposeAttachmentItem | ComposeInlineImage, inline: boolean) => {
    if (item.status === 'saving' || item.status === 'removing') return
    const removeLocal = () => {
      if (inline && 'previewUrl' in item && item.previewUrl) URL.revokeObjectURL(item.previewUrl)
      if (inline) setInlineImages((current) => {
        const next = current.filter((entry) => entry.localId !== item.localId)
        inlineImagesRef.current = next
        return next
      })
      else setAttachments((current) => {
        const next = current.filter((entry) => entry.localId !== item.localId)
        attachmentsRef.current = next
        return next
      })
    }
    if (!isNativeRuntime || !accountId || !draftIdRef.current || !item.persistedId) {
      removeLocal()
      return
    }
    const mark = <T extends ComposeAttachmentItem>(current: T[], status: ComposeAttachmentStatus) => current.map((entry) => (
      entry.localId === item.localId ? { ...entry, status } : entry
    ))
    if (inline) setInlineImages((current) => {
      const next = mark(current, 'removing')
      inlineImagesRef.current = next
      return next
    })
    else setAttachments((current) => {
      const next = mark(current, 'removing')
      attachmentsRef.current = next
      return next
    })
    try {
      const draft = await invoke<NativeDraft>('drafts.attachment.remove', {
        accountId,
        draftId: draftIdRef.current,
        attachmentId: item.persistedId,
      }, 30_000)
      removeLocal()
      setDraftStatus('附件已从草稿移除')
      onDraftChanged?.(draft)
    } catch (error) {
      if (inline) setInlineImages((current) => {
        const next = mark(current, 'saved')
        inlineImagesRef.current = next
        return next
      })
      else setAttachments((current) => {
        const next = mark(current, 'saved')
        attachmentsRef.current = next
        return next
      })
      onError(error instanceof Error ? error.message : '附件移除失败')
    }
  }

  const requestClose = useCallback(async () => {
    if (isSending) return
    try {
      await waitForAttachmentJobs()
      const hasFailedAttachment = [...attachmentsRef.current, ...inlineImagesRef.current]
        .some((attachment) => attachment.status === 'failed')
      if (hasFailedAttachment) {
        setDraftStatus('有附件尚未安全保存；请移除后再关闭，或直接发送')
        onError('有附件未保存，为避免丢失，撰写窗口暂未关闭')
        return
      }
      const hasContent = [to, cc, bcc, subject, body].some((value) => value.trim())
      const currentDraftId = draftIdRef.current
      if (!hasContent && attachmentsRef.current.length === 0 && inlineImagesRef.current.length === 0 && isNativeRuntime && accountId && currentDraftId) {
        await invoke('drafts.remove', { accountId, id: currentDraftId }, 30_000)
        onDraftRemoved?.(currentDraftId)
        draftIdRef.current = undefined
        onClose()
        return
      }
      await saveDraftSnapshot(false)
      onClose()
    } catch (error) {
      setDraftStatus('关闭前保存失败，请重试')
      onError(error instanceof Error ? error.message : '关闭前保存草稿失败')
    }
  }, [accountId, bcc, body, cc, isNativeRuntime, isSending, onClose, onDraftRemoved, onError, saveDraftSnapshot, subject, to, waitForAttachmentJobs])

  useEffect(() => {
    const handleEscape = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      if (event.target instanceof HTMLElement && event.target.closest('.rich-compose-link, .compose-schedule-menu')) return
      event.preventDefault()
      event.stopImmediatePropagation()
      void requestClose()
    }
    window.addEventListener('keydown', handleEscape, true)
    return () => window.removeEventListener('keydown', handleEscape, true)
  }, [requestClose])

  const discard = async () => {
    await waitForAttachmentJobs()
    const currentDraftId = draftIdRef.current
    if (!currentDraftId) {
      onClose()
      return
    }
    setSending(true)
    try {
      if (isNativeRuntime && accountId) {
        await invoke('drafts.remove', { accountId, id: currentDraftId }, 30_000)
        onDraftRemoved?.(currentDraftId)
      }
      onClose()
    } catch (error) {
      onError(error instanceof Error ? error.message : '草稿丢弃失败，请稍后重试')
    } finally {
      setSending(false)
    }
  }

  const send = async (scheduledFor?: number) => {
    if (!to.trim().includes('@')) {
      onError('请输入有效的收件人地址')
      return
    }
    if (isNativeRuntime && !accountId) {
      onError('请先添加一个可发送邮件的账户')
      return
    }
    setSending(true)
    const uploadIds: string[] = []
    let sendResult: NativeSendResponse | undefined
    try {
      if (isNativeRuntime) {
        await waitForAttachmentJobs()
        await saveDraftSnapshot(true)
        const currentAttachments = attachmentsRef.current
        const currentInlineImages = inlineImagesRef.current
        const allItems = [...currentAttachments, ...currentInlineImages]
        const missingAttachments = allItems.filter((item) => !item.persistedId && !item.file)
        if (missingAttachments.length) throw new Error('部分草稿附件无法读取，请移除后重试')
        const fallbackItems = allItems.filter((item): item is typeof item & { file: File } => !item.persistedId && Boolean(item.file))
        let cursor = 0
        const uploadedByIndex: string[] = []
        const worker = async () => {
          while (cursor < fallbackItems.length) {
            const index = cursor++
            const item = fallbackItems[index]
            setUploadingName('正在上传 ' + item.fileName)
            const contentId = typeof (item as Partial<ComposeInlineImage>).contentId === 'string'
              ? (item as ComposeInlineImage).contentId
              : undefined
            uploadedByIndex[index] = await uploadAttachment(item.file, contentId)
          }
        }
        await Promise.all(Array.from({ length: Math.min(3, fallbackItems.length) }, () => worker()))
        uploadIds.push(...uploadedByIndex)
        const persistedIds = allItems.flatMap((item) => item.persistedId ? [item.persistedId] : [])
        const currentDraftId = draftIdRef.current
        const effectiveHtml = htmlMode || currentInlineImages.length > 0
        const outgoingBody = appendAccountSignature(body, accountSignature)
        sendResult = await invoke<NativeSendResponse>('mail.send', {
          accountId,
          to: to.trim(),
          ...(cc.trim() ? { cc: cc.trim() } : {}),
          ...(bcc.trim() ? { bcc: bcc.trim() } : {}),
          subject: subject.trim() || '(无主题)',
          textBody: outgoingBody,
          ...(inReplyTo ? { inReplyTo } : {}),
          ...(references.length ? { references } : {}),
          ...(effectiveHtml && (outgoingBody.trim() || currentInlineImages.length) ? {
            htmlBody: composeHtmlBody(body, currentInlineImages, htmlMode ? richBody : undefined, accountSignature),
          } : {}),
          ...(uploadIds.length ? { attachmentIds: uploadIds } : {}),
          ...(currentDraftId ? { draftId: currentDraftId } : {}),
          ...(persistedIds.length ? { draftAttachmentIds: persistedIds } : {}),
          ...(scheduledFor ? { scheduledFor } : {}),
        })
      } else {
        await new Promise((resolve) => window.setTimeout(resolve, 700))
        sendResult = scheduledFor
          ? {
              sent: false,
              queued: true,
              accountId: accountId ?? 'demo-account',
              outboxId: `demo-${Date.now()}`,
              scheduled: true,
              scheduledFor,
              undoable: false,
            }
          : {
              sent: false,
              queued: true,
              accountId: accountId ?? 'demo-account',
              outboxId: `demo-${Date.now()}`,
              undoable: true,
              undoSeconds: DEFAULT_UNDO_SEND_SECONDS,
              undoExpiresAt: Date.now() + DEFAULT_UNDO_SEND_SECONDS * 1_000,
            }
      }
      const sentDraftId = draftIdRef.current
      if (isNativeRuntime && accountId && sentDraftId && !sendResult.queued) {
        await invoke('drafts.remove', { accountId, id: sentDraftId }, 30_000).catch(() => undefined)
        onDraftRemoved?.(sentDraftId)
      }
      onSent(sendResult)
    } catch (error) {
      await mapWithConcurrency(uploadIds, ACCOUNT_IPC_CONCURRENCY, (uploadId) => invoke('mail.attachment.upload.cancel', { uploadId }).catch(() => undefined))
      onError(error instanceof Error ? error.message : '邮件发送失败，请稍后重试')
    } finally {
      setUploadingName('')
      setSending(false)
    }
  }

  const composeTitle = mode === 'reply' ? '回复邮件' : mode === 'reply-all' ? '回复全部' : mode === 'forward' ? '转发邮件' : '新邮件'
  const htmlEnabled = htmlMode || inlineImages.length > 0
  return <motion.div className="modal-backdrop compose-backdrop" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={(event) => { if (event.target === event.currentTarget) void requestClose() }}>
    <motion.div className="compose-modal" initial={{ opacity: 0, y: 20 }} animate={{ opacity: 1 }} exit={{ opacity: 0, y: 20 }}>
      <div className="compose-header"><strong>{composeTitle}</strong><div><TooltipButton label="最小化撰写窗口"><span className="window-minimize" /></TooltipButton><TooltipButton label="关闭撰写窗口" onClick={() => { void requestClose() }}><Icon name="close" size={17} /></TooltipButton></div></div>
      <div className="compose-recipient-row"><RecipientInput fieldId="compose-to" label="收件人" autoFocus value={to} onChange={setTo} accountId={accountId} senderEmail={senderEmail} placeholder="姓名或邮箱，可用逗号分隔多个地址" /><button type="button" className="copy-fields-button" onClick={() => setShowCopyFields((value) => !value)} aria-expanded={showCopyFields}>{showCopyFields ? '隐藏抄送' : '抄送 / 密送'}</button></div>
      {showCopyFields && <><RecipientInput fieldId="compose-cc" label="抄送" value={cc} onChange={setCc} accountId={accountId} senderEmail={senderEmail} placeholder="输入姓名或邮箱" /><RecipientInput fieldId="compose-bcc" label="密送" value={bcc} onChange={setBcc} accountId={accountId} senderEmail={senderEmail} placeholder="输入姓名或邮箱" /></>}
      <label>主题<input value={subject} onChange={(event) => setSubject(event.target.value)} placeholder="主题" /></label>
      {htmlEnabled
        ? <RichTextEditor value={richBody || plainTextToComposeHtml(body)} placeholder="输入内容，将以 HTML + 纯文本双格式发送…" onChange={(html, plainText) => { setRichBody(html); setBody(plainText) }} onError={onError} />
        : <textarea className="compose-body" value={body} onChange={(event) => setBody(event.target.value)} placeholder="写下你的邮件…" />}
      {accountSignature && <div className="compose-signature-preview"><span>账户签名 · 发送时自动加入</span><p>{accountSignature}</p></div>}
      {draftStatus && <div className="compose-draft-status" aria-live="polite"><Icon name="cloud" size={14} />{draftStatus}</div>}
      {htmlEnabled && <div className="compose-format-note"><Icon name="grid" size={14} />将发送安全的 HTML + 纯文本版本{inlineImages.length > 0 ? '，内嵌图片会显示在正文末尾' : ''}</div>}
      {inlineImages.length > 0 && <div className="compose-inline-images" aria-label="待发送内嵌图片">{inlineImages.map((image) => <div className={`compose-inline-image is-${image.status}`} key={image.localId}>{image.previewUrl ? <img src={image.previewUrl} alt={image.fileName} /> : <span className="compose-inline-placeholder"><Icon name="image" size={22} /></span>}<span>{image.fileName}<small>{image.status === 'saving' ? '加密保存中…' : image.status === 'failed' ? '保存失败' : image.status === 'removing' ? '移除中…' : '已保存'}</small></span><button type="button" disabled={image.status === 'saving' || image.status === 'removing'} onClick={() => { trackAttachmentJob(removeComposeAttachment(image, true)) }} aria-label={'移除内嵌图片 ' + image.fileName}>×</button></div>)}</div>}
      {attachments.length > 0 && <div className="compose-attachments" aria-label="待发送附件">{attachments.map((attachment) => <div className={`compose-attachment is-${attachment.status}`} key={attachment.localId}><span>{attachment.fileName}</span><small>{Math.max(1, Math.round(attachment.size / 1024))} KB · {attachment.status === 'saving' ? '加密保存中…' : attachment.status === 'failed' ? '保存失败' : attachment.status === 'removing' ? '移除中…' : '已保存'}</small><button type="button" disabled={attachment.status === 'saving' || attachment.status === 'removing'} onClick={() => { trackAttachmentJob(removeComposeAttachment(attachment, false)) }} aria-label={'移除附件 ' + attachment.fileName}>×</button></div>)}</div>}
      {uploadingName && <div className="compose-uploading" aria-live="polite"><Icon name="rotate" size={14} />{uploadingName}</div>}
      <div className="compose-footer"><div><TooltipButton label="添加附件" onClick={() => fileInputRef.current?.click()}><Icon name="paperclip" size={19} /></TooltipButton><input ref={fileInputRef} type="file" multiple hidden onChange={addFiles} /><TooltipButton label="插入图片" onClick={() => imageInputRef.current?.click()}><Icon name="image" size={19} /></TooltipButton><input ref={imageInputRef} type="file" accept="image/*" multiple hidden onChange={addInlineImages} /><TooltipButton label={htmlEnabled ? (inlineImages.length > 0 ? '内嵌图片需要 HTML 格式' : '切换为纯文本（移除格式）') : '启用富文本格式'} active={htmlEnabled} onClick={() => { if (inlineImages.length > 0) { onError('已插入内嵌图片，HTML 格式必须保持开启'); return } if (htmlMode) { setRichBody(''); setHtmlMode(false) } else { setRichBody(plainTextToComposeHtml(body)); setHtmlMode(true) } }}><span className="reply-a">A</span></TooltipButton></div><div className="compose-send-actions">{draftId && <button type="button" className="danger-button" onClick={() => { void discard() }} disabled={isSending}><Icon name="trash" size={16} />丢弃草稿</button>}<ScheduleSendControl disabled={isSending || (isNativeRuntime && !draftReady)} label={isSending ? (uploadingName || '发送中…') : isNativeRuntime && !draftReady ? '准备中…' : '发送'} onSendNow={() => { void send() }} onSchedule={(timestamp) => { void send(timestamp) }} /></div></div>
    </motion.div>
  </motion.div>
}
