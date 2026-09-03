import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const read = (path) => readFileSync(resolve(root, path), 'utf8')
const app = read('src/App.tsx')
const types = read('src/types.ts')
const cache = read('native/src/cache_db.rs')
const main = read('native/src/main.rs')
const sync = read('native/src/sync.rs')
const release = read('scripts/release-windows.ps1')

assert.match(cache, /pub fn mailbox_revision\(/)
assert.match(cache, /static BACKUP_REFRESH_RUNNING: AtomicBool/)
assert.match(cache, /pub fn spawn_backup_refresh\(cache_root: PathBuf\)/)
assert.match(cache, /compare_exchange\(false, true, Ordering::AcqRel, Ordering::Acquire\)/)
assert.match(cache, /\.name\("mailgo-cache-backup"\.to_string\(\)\)/)
assert.match(cache, /if !backup_is_due\(&cache_root\)/)
assert.match(sync, /session\.logout\(\)\.ok\(\);\s*crate::cache_db::spawn_backup_refresh\(cache_root\.to_path_buf\(\)\);/)
assert.doesNotMatch(sync, /cache_db::refresh_backup\(cache_root\)/)
assert.match(cache, /validate_identity\(&metadata, account_id, folder\)\?/)
assert.match(sync, /pub fn mailbox_revision\([\s\S]*?validate_mailbox_name\(folder\)\?/)
assert.match(main, /message\.payload\.get\("knownRevision"\)/)
assert.match(main, /before_uid\.is_none\(\)[\s\S]*?sync::mailbox_revision/)
assert.match(main, /"unchanged": true/)

assert.match(types, /unchanged\?: boolean/)
assert.match(app, /const mailboxMetaRef = useRef/)
assert.match(app, /const backgroundStatusRefreshRunningRef = useRef/)
assert.match(app, /backgroundStatusRefreshRunningRef\.current\) return/)
assert.match(app, /\{ knownRevision \}/)
assert.match(app, /result\.unchanged \|\| !result\.mailbox/)
assert.match(app, /finally \{\s*backgroundStatusRefreshRunningRef\.current = false/)
assert.match(app, /const refreshedAccounts = new Map/)
assert.match(app, /return changed \? next : current/)
assert.match(app, /sameStringArrayRecord\(current, refreshedFolders\) \? current : refreshedFolders/)
assert.match(app, /sameNestedStringRecord\(current, refreshedFolderLabels\) \? current : refreshedFolderLabels/)

for (const command of ['test:undo-send', 'test:outbox', 'test:conditional-refresh']) {
  assert.match(release, new RegExp(`npm run ${command.replace(':', '\\:')}`))
}

console.log('Revision-gated mailbox polling, asynchronous backup maintenance, and release-gate coverage checks passed.')
