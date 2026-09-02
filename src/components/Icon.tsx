import Add from 'reicon-react/icons/Add'
import AddCircle from 'reicon-react/icons/AddCircle'
import Archive from 'reicon-react/icons/Archive'
import ArrowRotate from 'reicon-react/icons/ArrowRotate'
import AttachCircle from 'reicon-react/icons/AttachCircle'
import AtSign from 'reicon-react/icons/AtSign'
import Bell from 'reicon-react/icons/Bell'
import Bold from 'reicon-react/icons/Bold'
import Brush from 'reicon-react/icons/Brush'
import Check from 'reicon-react/icons/Check'
import CheckCircle from 'reicon-react/icons/CheckCircle'
import Clock from 'reicon-react/icons/Clock'
import CloseCircle from 'reicon-react/icons/CloseCircle'
import Cloud from 'reicon-react/icons/Cloud'
import Copy from 'reicon-react/icons/Copy'
import DarkLight from 'reicon-react/icons/DarkLight'
import DocumentText from 'reicon-react/icons/DocumentText'
import Download from 'reicon-react/icons/Download'
import Edit from 'reicon-react/icons/Edit'
import Eye from 'reicon-react/icons/Eye'
import EyeSlash from 'reicon-react/icons/EyeSlash'
import Filter from 'reicon-react/icons/Filter'
import Folder from 'reicon-react/icons/Folder'
import Forward from 'reicon-react/icons/Forward'
import Grid from 'reicon-react/icons/Grid'
import Help from 'reicon-react/icons/Help'
import Image from 'reicon-react/icons/Image'
import Inbox from 'reicon-react/icons/Inbox'
import InfoCircle from 'reicon-react/icons/InfoCircle'
import Key from 'reicon-react/icons/Key'
import Link from 'reicon-react/icons/Link'
import Lock from 'reicon-react/icons/Lock'
import Maximize from 'reicon-react/icons/Maximize'
import Menu from 'reicon-react/icons/Menu'
import Message from 'reicon-react/icons/Message'
import More from 'reicon-react/icons/More'
import Moon from 'reicon-react/icons/Moon'
import OrderedList from 'reicon-react/icons/OrderedList'
import Paperclip from 'reicon-react/icons/Paperclip'
import Refresh from 'reicon-react/icons/Refresh'
import Reply from 'reicon-react/icons/Reply'
import Search from 'reicon-react/icons/Search'
import Send from 'reicon-react/icons/Send'
import Setting from 'reicon-react/icons/Setting'
import Shield from 'reicon-react/icons/Shield'
import ShieldCheck from 'reicon-react/icons/ShieldCheck'
import Star from 'reicon-react/icons/Star'
import Trash from 'reicon-react/icons/Trash'
import TextUnderline from 'reicon-react/icons/TextUnderline'
import Italic from 'reicon-react/icons/Italic'
import UnorderedList from 'reicon-react/icons/UnorderedList'
import QuoteDown from 'reicon-react/icons/QuoteDown'
import User from 'reicon-react/icons/User'

const icons = {
  add: Add,
  addCircle: AddCircle,
  archive: Archive,
  rotate: ArrowRotate,
  attachment: AttachCircle,
  at: AtSign,
  bell: Bell,
  bold: Bold,
  brush: Brush,
  check: Check,
  checkCircle: CheckCircle,
  clock: Clock,
  close: CloseCircle,
  cloud: Cloud,
  copy: Copy,
  theme: DarkLight,
  document: DocumentText,
  download: Download,
  edit: Edit,
  eye: Eye,
  eyeSlash: EyeSlash,
  filter: Filter,
  folder: Folder,
  forward: Forward,
  grid: Grid,
  help: Help,
  image: Image,
  italic: Italic,
  inbox: Inbox,
  info: InfoCircle,
  key: Key,
  link: Link,
  lock: Lock,
  maximize: Maximize,
  menu: Menu,
  message: Message,
  more: More,
  moon: Moon,
  orderedList: OrderedList,
  paperclip: Paperclip,
  refresh: Refresh,
  reply: Reply,
  search: Search,
  send: Send,
  settings: Setting,
  shield: Shield,
  shieldCheck: ShieldCheck,
  star: Star,
  trash: Trash,
  underline: TextUnderline,
  unorderedList: UnorderedList,
  quote: QuoteDown,
  user: User,
} as const

export type IconName = keyof typeof icons

interface IconProps {
  name: IconName
  size?: number | string
  weight?: 'Filled' | 'Outline'
  className?: string
  color?: string
}

export function Icon({ name, size = 20, weight = 'Outline', className, color }: IconProps) {
  const Component = icons[name]
  return <Component aria-hidden="true" size={size} weight={weight} className={className} color={color} />
}
