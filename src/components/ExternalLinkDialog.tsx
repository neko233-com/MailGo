import { motion } from 'motion/react'
import { useEffect, useRef, useState } from 'react'
import type { ExternalLinkInspection } from '../linkSafety'
import { Icon } from './Icon'

type ExternalLinkDialogProps = {
  inspection: ExternalLinkInspection
  onClose: () => void
  onOpen: (url: string) => Promise<void>
  onError: (message: string) => void
}

export function ExternalLinkDialog({ inspection, onClose, onOpen, onError }: ExternalLinkDialogProps) {
  const [isOpening, setOpening] = useState(false)
  const dialogRef = useRef<HTMLDivElement>(null)
  const cancelRef = useRef<HTMLButtonElement>(null)

  useEffect(() => {
    const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null
    cancelRef.current?.focus()
    return () => {
      if (previousFocus?.isConnected) previousFocus.focus()
    }
  }, [])

  const keepFocusInside = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== 'Tab') return
    const controls = Array.from(dialogRef.current?.querySelectorAll<HTMLButtonElement>('button:not(:disabled)') ?? [])
    if (controls.length === 0) return
    const first = controls[0]
    const last = controls[controls.length - 1]
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault()
      last.focus()
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault()
      first.focus()
    }
  }

  const open = async () => {
    if (isOpening) return
    setOpening(true)
    try {
      await onOpen(inspection.url)
      onClose()
    } catch (error) {
      onError(error instanceof Error ? error.message : '无法打开邮件链接')
      setOpening(false)
    }
  }

  return (
    <motion.div
      className="modal-backdrop external-link-backdrop"
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      exit={{ opacity: 0 }}
      onMouseDown={(event) => { if (!isOpening && event.target === event.currentTarget) onClose() }}
    >
      <motion.div
        ref={dialogRef}
        className={`external-link-modal is-${inspection.risk}`}
        initial={{ opacity: 0, y: 10, scale: 0.985 }}
        animate={{ opacity: 1, y: 0, scale: 1 }}
        exit={{ opacity: 0, y: 6, scale: 0.985 }}
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="external-link-title"
        aria-describedby="external-link-description"
        onKeyDown={keepFocusInside}
      >
        <div className="modal-header">
          <div><Icon name={inspection.risk === 'caution' ? 'shield' : 'shieldCheck'} size={20} /><h2 id="external-link-title">核对外部链接</h2></div>
          <button className="icon-button" type="button" aria-label="取消打开链接" title="取消打开链接" onClick={onClose} disabled={isOpening}><Icon name="close" size={18} /></button>
        </div>
        <div className="external-link-body">
          <p id="external-link-description">邮件中的链接可能伪装显示文字。继续前，请核对实际目标：</p>
          <div className="external-link-target">
            <span><Icon name={inspection.kind === 'https' ? 'link' : 'at'} size={17} />{inspection.kind === 'https' ? '实际网站' : '实际收件人'}</span>
            <strong><bdi dir="ltr">{inspection.primaryLabel}</bdi></strong>
            <small><bdi dir="ltr">{inspection.secondaryLabel}</bdi></small>
          </div>
          {inspection.hasHiddenParameters && <div className="external-link-privacy-note"><Icon name="info" size={15} /><span>链接还包含查询参数或页面锚点；为保护隐私，此处不展开显示。</span></div>}
          {inspection.warnings.length > 0 && <div className="external-link-warnings" role="status"><strong><Icon name="shield" size={16} />需要额外留意</strong>{inspection.warnings.map((warning) => <p key={warning}>{warning}</p>)}</div>}
          <p className="external-link-footnote">MailGo 不会预先访问该地址。只有点击“{inspection.kind === 'https' ? '继续打开' : '打开邮件应用'}”后，才会交给 Windows 处理。</p>
        </div>
        <div className="modal-footer external-link-actions">
          <span><Icon name="lock" size={16} />未连接到外部网站</span>
          <div><button ref={cancelRef} className="secondary-button" type="button" onClick={onClose} disabled={isOpening}>取消</button><button className="gradient-button" type="button" onClick={() => { void open() }} disabled={isOpening}>{isOpening && <span className="loading-spinner loading-spinner-small" aria-hidden="true" />}{isOpening ? '正在打开…' : inspection.kind === 'https' ? '继续打开' : '打开邮件应用'}</button></div>
        </div>
      </motion.div>
    </motion.div>
  )
}
