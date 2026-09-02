import type { CSSProperties } from 'react'
import { providerDefinitions } from '../data'
import type { Provider } from '../types'

export function ProviderMark({ provider, size = 'md' }: { provider: Provider; size?: 'sm' | 'md' | 'lg' }) {
  const definition = providerDefinitions.find((item) => item.id === provider) ?? providerDefinitions[3]
  return (
    <span className={`provider-mark provider-${provider} provider-mark-${size}`} style={{ '--provider-accent': definition.accent } as CSSProperties}>
      {provider === 'google' ? <span className="google-mark">G</span> : provider === 'outlook' ? <span className="outlook-mark">O</span> : provider === 'qq' ? <span className="qq-mark">Q</span> : '@'}
    </span>
  )
}
