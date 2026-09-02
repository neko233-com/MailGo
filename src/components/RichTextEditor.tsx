import { useCallback, useLayoutEffect, useRef, useState } from 'react'
import { Icon, type IconName } from './Icon'
import {
  composeHtmlBytes,
  composeHtmlToPlainText,
  MAX_COMPOSE_HTML_BYTES,
  normalizeComposeLink,
  plainTextToComposeHtml,
  sanitizeComposeHtml,
} from '../richText'

type RichTextEditorProps = {
  value: string
  placeholder: string
  onChange: (html: string, plainText: string) => void
  onError: (message: string) => void
}

type EditorCommand = 'bold' | 'italic' | 'underline' | 'insertUnorderedList' | 'insertOrderedList' | 'formatBlock' | 'removeFormat'

function EditorButton({ label, icon, text, onActivate }: { label: string; icon?: IconName; text?: string; onActivate: () => void }) {
  return <button type="button" aria-label={label} title={label} onMouseDown={(event) => event.preventDefault()} onClick={onActivate}>
    {icon ? <Icon name={icon} size={15} /> : <span>{text}</span>}
  </button>
}

function selectedEditorRange(editor: HTMLElement) {
  const selection = window.getSelection()
  if (!selection?.rangeCount) return undefined
  const range = selection.getRangeAt(0)
  return editor.contains(range.commonAncestorContainer) ? range.cloneRange() : undefined
}

function restoreRange(editor: HTMLElement, range: Range | undefined) {
  editor.focus()
  if (!range) return
  const selection = window.getSelection()
  selection?.removeAllRanges()
  selection?.addRange(range)
}

export function RichTextEditor({ value, placeholder, onChange, onError }: RichTextEditorProps) {
  const editorRef = useRef<HTMLDivElement>(null)
  const localValueRef = useRef('')
  const savedRangeRef = useRef<Range | undefined>(undefined)
  const linkInputRef = useRef<HTMLInputElement>(null)
  const [linkOpen, setLinkOpen] = useState(false)
  const [linkValue, setLinkValue] = useState('')

  useLayoutEffect(() => {
    if (value === localValueRef.current) return
    const safeValue = sanitizeComposeHtml(value)
    localValueRef.current = safeValue
    if (editorRef.current) editorRef.current.innerHTML = safeValue
  }, [value])

  const publish = useCallback((normalizeEditor = false) => {
    const editor = editorRef.current
    if (!editor) return
    const safeValue = sanitizeComposeHtml(editor.innerHTML)
    if (composeHtmlBytes(safeValue) > MAX_COMPOSE_HTML_BYTES) {
      editor.innerHTML = localValueRef.current
      onError('富文本正文不能超过 2 MB')
      return
    }
    localValueRef.current = safeValue
    if (normalizeEditor && editor.innerHTML !== safeValue) editor.innerHTML = safeValue
    onChange(safeValue, composeHtmlToPlainText(safeValue))
  }, [onChange, onError])

  const applyCommand = useCallback((command: EditorCommand, argument?: string) => {
    const editor = editorRef.current
    if (!editor) return
    editor.focus()
    document.execCommand(command, false, argument)
    publish()
  }, [publish])

  const openLink = () => {
    const editor = editorRef.current
    if (!editor) return
    savedRangeRef.current = selectedEditorRange(editor)
    if (!savedRangeRef.current) {
      onError('请先在正文中选择文字，或把光标放在要插入链接的位置')
      return
    }
    setLinkValue('')
    setLinkOpen(true)
    window.setTimeout(() => linkInputRef.current?.focus(), 0)
  }

  const insertLink = (event: React.FormEvent) => {
    event.preventDefault()
    const editor = editorRef.current
    const href = normalizeComposeLink(linkValue)
    if (!editor || !href) {
      onError('链接必须是安全的 HTTPS 或 mailto 地址')
      return
    }
    const range = savedRangeRef.current
    restoreRange(editor, range)
    if (range?.collapsed) {
      document.execCommand('insertHTML', false, `<a href="${href.replace(/&/g, '&amp;').replace(/"/g, '&quot;')}">${href.replace(/&/g, '&amp;').replace(/</g, '&lt;')}</a>`)
    } else {
      document.execCommand('createLink', false, href)
    }
    setLinkOpen(false)
    setLinkValue('')
    savedRangeRef.current = undefined
    publish()
  }

  const handlePaste = (event: React.ClipboardEvent<HTMLDivElement>) => {
    event.preventDefault()
    const clipboardHtml = event.clipboardData.getData('text/html')
    const clipboardText = event.clipboardData.getData('text/plain')
    const safeHtml = clipboardHtml ? sanitizeComposeHtml(clipboardHtml) : plainTextToComposeHtml(clipboardText)
    if (composeHtmlBytes(safeHtml) > MAX_COMPOSE_HTML_BYTES) {
      onError('粘贴内容过大，富文本正文最多 2 MB')
      return
    }
    document.execCommand('insertHTML', false, safeHtml)
    publish()
  }

  const handleKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (!(event.ctrlKey || event.metaKey)) return
    const command = event.key.toLowerCase() === 'b' ? 'bold'
      : event.key.toLowerCase() === 'i' ? 'italic'
        : event.key.toLowerCase() === 'u' ? 'underline'
          : undefined
    if (!command) return
    event.preventDefault()
    applyCommand(command)
  }

  return <div className="rich-compose">
    <div className="rich-compose-toolbar" role="toolbar" aria-label="正文格式">
      <EditorButton label="粗体 (Ctrl+B)" icon="bold" onActivate={() => applyCommand('bold')} />
      <EditorButton label="斜体 (Ctrl+I)" icon="italic" onActivate={() => applyCommand('italic')} />
      <EditorButton label="下划线 (Ctrl+U)" icon="underline" onActivate={() => applyCommand('underline')} />
      <span className="rich-compose-divider" />
      <EditorButton label="无序列表" icon="unorderedList" onActivate={() => applyCommand('insertUnorderedList')} />
      <EditorButton label="有序列表" icon="orderedList" onActivate={() => applyCommand('insertOrderedList')} />
      <EditorButton label="引用" icon="quote" onActivate={() => applyCommand('formatBlock', 'blockquote')} />
      <span className="rich-compose-divider" />
      <EditorButton label="插入链接" icon="link" onActivate={openLink} />
      <EditorButton label="清除格式" text="Tx" onActivate={() => applyCommand('removeFormat')} />
      {linkOpen && <form className="rich-compose-link" onSubmit={insertLink}>
        <input ref={linkInputRef} value={linkValue} onChange={(event) => setLinkValue(event.target.value)} placeholder="https://example.com" aria-label="链接地址" onKeyDown={(event) => {
          if (event.key !== 'Escape') return
          event.preventDefault()
          event.stopPropagation()
          setLinkOpen(false)
          if (editorRef.current) restoreRange(editorRef.current, savedRangeRef.current)
        }} />
        <button type="submit">插入</button>
      </form>}
    </div>
    <div
      ref={editorRef}
      className="compose-body rich-compose-body"
      contentEditable
      suppressContentEditableWarning
      role="textbox"
      aria-label="邮件正文"
      aria-multiline="true"
      data-placeholder={placeholder}
      onInput={() => publish()}
      onBlur={() => { if (!linkOpen) publish(true) }}
      onPaste={handlePaste}
      onKeyDown={handleKeyDown}
    />
  </div>
}
