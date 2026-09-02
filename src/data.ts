import type { ProviderDefinition } from './types'

export const providerDefinitions: ProviderDefinition[] = [
  {
    id: 'google',
    label: 'Google',
    description: 'Gmail / Google Workspace',
    accent: '#4285f4',
    icon: 'G',
    authUrl: 'https://myaccount.google.com/apppasswords',
    guide: ['打开 Google 账户安全设置', '开启两步验证后进入应用专用密码', '生成 MailGo 专用授权凭据'],
    requiresAuthCode: true,
  },
  {
    id: 'qq',
    label: 'QQ 邮箱',
    description: '支持多个 QQ 邮箱并行同步',
    accent: '#38a9f9',
    icon: 'Q',
    authUrl: 'https://mail.qq.com/',
    guide: ['打开 QQ 邮箱设置', '进入账户安全并开启 POP3/IMAP 服务', '生成授权码并复制到 MailGo'],
    requiresAuthCode: true,
  },
  {
    id: 'outlook',
    label: 'Outlook',
    description: 'Outlook.com / Microsoft 365 · 设备授权',
    accent: '#2f80ed',
    icon: 'O',
    authUrl: 'https://microsoft.com/devicelogin',
    guide: ['打开 Microsoft 设备验证页面', '输入 MailGo 显示的设备代码', '完成账户授权后返回 MailGo'],
    requiresAuthCode: false,
  },
  {
    id: 'other',
    label: '其他邮箱',
    description: '自定义 IMAP / SMTP 服务器',
    accent: '#a5aec7',
    icon: '@',
    authUrl: 'https://support.google.com/mail/answer/7126229',
    guide: ['准备邮箱服务商的 IMAP / SMTP 参数', '确认已开启第三方客户端服务', '输入邮箱地址与授权凭据'],
    requiresAuthCode: true,
  },
]

export const folderLabels = [
  { id: 'inbox' as const, label: '收件箱', icon: 'inbox', unread: 12 },
  { id: 'starred' as const, label: '星标', icon: 'star', unread: 0 },
  { id: 'snoozed' as const, label: '稍后处理', icon: 'clock', unread: 0 },
  { id: 'outbox' as const, label: '发件箱', icon: 'clock', unread: 0 },
  { id: 'sent' as const, label: '已发送', icon: 'send', unread: 0 },
  { id: 'drafts' as const, label: '草稿箱', icon: 'document', unread: 3 },
  { id: 'archive' as const, label: '归档', icon: 'archive', unread: 0 },
  { id: 'spam' as const, label: '垃圾邮件', icon: 'shield', unread: 4 },
  { id: 'trash' as const, label: '回收站', icon: 'trash', unread: 0 },
]
