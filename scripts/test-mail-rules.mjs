import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import ts from 'typescript'

const root = resolve(import.meta.dirname, '..')
const read = (path) => readFileSync(resolve(root, path), 'utf8')
const source = read('src/mailRules.ts')
const output = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2022, target: ts.ScriptTarget.ES2022 },
}).outputText
const mailRules = await import(`data:text/javascript;base64,${Buffer.from(output).toString('base64')}`)

assert.equal(mailRules.normalizeRuleSender(' News@Example.COM '), 'news@example.com')
assert.equal(mailRules.normalizeRuleDomain('@News.Example.COM.'), 'news.example.com')
assert.throws(() => mailRules.normalizeRuleSender('not-an-address'))
assert.throws(() => mailRules.normalizeRuleDomain('-invalid.example'))

const fixture = {
  id: 'fixture',
  accountId: 'account-1',
  folder: 'inbox',
  from: 'offer@news.example.com',
  senderName: 'Offer',
  subject: 'Fixture',
  preview: '',
  timestamp: '',
  dateGroup: '',
  unread: true,
  starred: false,
  accent: '#000',
  avatar: 'O',
  body: [],
}
const globalDomain = { id: 'domain-rule', kind: 'domain', value: 'example.com', createdAt: 1 }
const wrongAccount = { id: 'sender-rule', accountId: 'account-2', kind: 'sender', value: 'offer@news.example.com', createdAt: 2 }
assert.equal(mailRules.mailMatchesRule(fixture, globalDomain), true)
assert.equal(mailRules.mailMatchesRule(fixture, wrongAccount), false)
assert.deepEqual(
  { blocked: mailRules.applyMailRules(fixture, [globalDomain]).blocked, blockedRuleId: mailRules.applyMailRules(fixture, [globalDomain]).blockedRuleId },
  { blocked: true, blockedRuleId: 'domain-rule' },
)
assert.deepEqual(
  { blocked: mailRules.applyMailRules({ ...fixture, blocked: true, blockedRuleId: 'old' }, []).blocked, blockedRuleId: mailRules.applyMailRules({ ...fixture, blocked: true, blockedRuleId: 'old' }, []).blockedRuleId },
  { blocked: false, blockedRuleId: undefined },
)

const nativeRules = read('native/src/rules.rs')
const nativeMain = read('native/src/main.rs')
const nativeMail = read('native/src/mail.rs')
const nativeSync = read('native/src/sync.rs')
const app = read('src/App.tsx')
const component = read('src/components/MailRuleManager.tsx')
const styles = read('src/styles.css')
const release = read('scripts/release-windows.ps1')

for (const command of ['mail.rules.list', 'mail.rules.add', 'mail.rules.remove']) {
  assert.ok(nativeMain.includes(`"${command}"`), `missing native IPC command ${command}`)
}
assert.match(nativeMain, /rules::apply_to_messages[\s\S]*?active_mail_rules/)
assert.match(nativeMain, /rules::remove_account/)
assert.match(nativeRules, /protect_cache\(&payload\)/)
assert.match(nativeRules, /STORE_BACKUP_FILE/)
assert.match(nativeRules, /MAX_RULES: usize = 256/)
assert.match(nativeRules, /fn corrupt_primary_recovers_previous_encrypted_backup/)
assert.match(nativeRules, /domain\.ends_with\(&format!\("\.\{\}"/)
assert.match(nativeMail, /pub blocked: bool/)
assert.match(nativeMail, /pub blocked_rule_id: Option<String>/)
assert.match(nativeRules, /pub fn is_blocked/)
assert.match(nativeSync, /pub new_unread: usize/)
assert.match(nativeSync, /fn count_notifiable_new_unread/)
assert.match(nativeSync, /message\.unread && !crate::rules::is_blocked\(snapshot, message\)/)
assert.match(nativeSync, /result\.new_unread > 0/)
assert.match(nativeSync, /!crate::rules::is_blocked\(&mail_rules, &item\.message\)/)
assert.match(nativeSync, /fn new_mail_notifications_exclude_read_and_blocked_messages/)
assert.match(app, /mail\.blocked \|\| mail\.category === 'ads'/)
assert.match(app, /const blockMatch = !mail\.blocked \|\| selectedCategory === 'ads'/)
assert.match(app, /屏蔽此发件人/)
assert.match(app, /屏蔽该发件域名/)
assert.match(component, /加密保存在这台电脑/)
assert.match(component, /发件域名规则同时匹配其子域名/)
assert.match(styles, /\.mail-rule-modal/)
assert.match(styles, /\.app-shell\.is-compact-density \.mail-rule-modal/)
assert.match(release, /npm run test:mail-rules/)

console.log('Encrypted sender/domain rule matching, native overlays, and compact management UI checks passed.')
