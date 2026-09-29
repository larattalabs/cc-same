# How CC Same works

## Where Claude Desktop keeps your sessions

Claude Desktop (macOS and Windows; unofficial builds on Linux) keeps one index of local Code
sessions per **account and organization**:

```
<Claude data>/claude-code-sessions/<account-id>/<org-id>/
    local_<uuid>.json          one session: title, cwd, model, archive state, …
    deleted_<id>               "this session was deleted at <ms>" (id = session or transcript id)
    archived-sessions.idx      load-order hint: {"v":1,"archived":[…]}
    scheduled-tasks.json       Code scheduled tasks
    backlog/tasks.json         task suggestions
```

`<Claude data>` is `~/Library/Application Support/Claude` on macOS, `%APPDATA%\Claude` on
Windows (the Store build virtualises it as `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude`), and
`~/.config/Claude` on Linux. Local Cowork sessions live in the same shape under
`local-agent-mode-sessions/`, with a folder per session.

The conversations themselves are Claude Code transcripts in `~/.claude/projects/<project>/<id>.jsonl`,
shared by every account. A session record points at its transcript through `cliSessionId`.

Desktop reads the index of the account it signs in to, **only when that account loads**. So after
an account switch the sidebar is empty although every transcript is still on disk. It has been
reported since April 2026 ([anthropics/claude-code#48511](https://github.com/anthropics/claude-code/issues/48511),
closed as not planned; [#74662](https://github.com/anthropics/claude-code/issues/74662)), and it still
reproduces on Desktop 2.9939.2.

## Why not symlink the folders together

"Merge the folders, then symlink every account's folder to one of them" looks like the simplest fix,
and several tools do it. On current Desktop it breaks saving:

- Before writing a session, Desktop makes sure its storage folder exists with a hardened
  `mkdir`, which ends with `open(dir, O_RDONLY|O_DIRECTORY|O_NOFOLLOW)`.
- If the folder is a symlink, that `open` fails (`ENOTDIR` on macOS).
- The save routine catches the error, writes `Failed to save session …` to its log, and carries on.
  The session stays in memory and is gone after a restart.

Desktop also refuses (or warns about) files with more than one hard link, so hard links are out too.
Every account's index therefore has to stay a real folder with real files, and keeping them
identical means copying. `cc-same fix-symlinks` turns symlinked folders left by other tools
back into real ones.

## One writer at a time

Copying between folders that Desktop also writes would race it, except that Desktop only ever writes
**one** of them: the account it has loaded. CC Same builds on that:

1. It works out which account Desktop has loaded, from the newest
   `[LocalSessionManager] Initialization succeeded — accountId=…, orgId=…` line in Desktop's
   `main.log`, and `lastKnownAccountUuid` in its `config.json`. If Desktop is running but neither
   can be read, every index counts as loaded.
2. It never modifies an index Desktop has loaded. It copies that index's changes to all the
   others as they happen.
3. After you switch accounts or quit, the index you left is caught up. A just-left account stays
   read-only for 30 seconds while Desktop flushes it.
4. An index CC Same has never seen (an account signing in for the first time, or every account
   on the very first run) is the one exception: sessions it lacks can be added while it is loaded,
   because nothing in it is overwritten. Desktop shows them after one restart, and the app tells
   you when that is needed.

## Deciding what each index should contain

For every session:

- **Newest wins.** The copy with the newest file modification time wins, ties going to the
  greatest partition key, so every run agrees. Copies keep the source's mtime, so a copy never
  looks newer than what it was copied from, and nothing ping-pongs.
- **Healthy beats damaged.** If the newest copy lost its transcript link (`cliSessionId` gone or
  `transcriptUnavailable`) while an older copy points at a transcript that is still on disk, the
  older copy wins.
- **Deletions.** A `deleted_<session id>` marker newer than the session's newest copy means the
  session was deleted after its last change. It is moved to the trash everywhere, and the markers
  are copied everywhere, which also stops Desktop's importer from offering the session again. A
  session newer than its marker has come back, so the stale markers go to the trash, as Desktop
  does itself when a session returns.
- **Account-bound fields stay home.** These are never copied:
  - `remoteMcpServersConfig`, `enabledMcpTools` and `withheldConnectorHosts`: the organization's
    connectors.
  - `bridgeSessionId(s)`, `steeredByRemoteClient` and `remoteControl*`: Remote Control mirrors are
    server sessions owned by an account.
  - `publishedArtifacts`.
  - `isStarred`: pins are star-synced with each account's server settings.
  - For Cowork: `emailAddress`, `spaceId`, `userSelectedProjectUuids`, and the account files in a
    session's folder (`.claude/.claude.json`, `policy-limits.json`, caches, `uploads-tmp`).

  Each account keeps its own values, and a new copy starts without them. Desktop reads all of
  these with defaults, so absent is safe. `remoteMcpServersConfig` is typically about 97% of a
  record, so the engine keeps only the portable part in memory and reads the target's own fields
  back at write time.
- **Collection files.** `scheduled-tasks.json` and `backlog/tasks.json` get a three-way merge
  against what each account had at the last sync. Lists of objects with an `id` merge item by item.
  An account's edits since its last sync win; an account that has never been synced can add items
  but not remove any; an item removed from the group stays removed. Desktop's indentation is kept.
- **Archive hint.** `archived-sessions.idx` is regenerated byte-for-byte the way Desktop writes it.

## Writing safely

- **Snapshots.** Before its first change, CC Same takes a baseline snapshot of every index and
  keeps it forever. After that it snapshots at most hourly and keeps the 20 newest. Each snapshot
  is a copy-on-write clone (APFS, Btrfs/XFS, ReFS; a plain copy elsewhere). `restore` puts a
  snapshot back (Claude must be closed) after taking a `pre-restore` snapshot.
- **Nothing is deleted.** Removed or replaced files go to `<state>/trash/<time>/…` for 30 days.
- **Atomic writes.** Every write goes to a temporary file in the same folder, is fsynced, gets its
  mtime, and is renamed into place. New files use a no-clobber rename, so a file Desktop created
  meanwhile is never overwritten. An existing file is only rewritten if its mtime still matches
  what was planned.
- **Hands off the unexpected.** Symlinks are never followed. Records whose `sessionId` does not
  match their file name, or that cannot be parsed, are left alone.
- **One run at a time**, via an exclusive lock (`flock` / `LockFileEx`) on `<state>/lock`.
- **Re-checking.** While applying, Desktop's state is re-checked every 250 ms, in case it launches
  or switches accounts mid-run.

## The background agent

`cc-same install` (or the app's switch) copies the binary to `<state>/bin` and registers it to run
`watch` at login: a LaunchAgent on macOS, a `Run` value on Windows (a windowless build, so nothing
flashes), and a systemd user service on Linux, with an XDG autostart fallback. The Mac app is the
exception: it registers its own executable where it is, because the app's signature covers that
file only inside `CC Same.app`, and macOS refuses to run a copy of it anywhere else (0.1.0 and
0.1.1 made that copy; the app sets such an agent up again when it opens). For the same reason it
won't set the agent up while it runs from a disk image. The agent:

- polls a cheap fingerprint of the index folders and Desktop's `config.json` every 2 seconds;
- waits up to 6 seconds for a burst of writes to settle, then syncs;
- runs a full pass every minute anyway;
- writes a heartbeat to `<state>/agent.json` every 15 seconds, which the app reads.

On macOS the LaunchAgent names the app it belongs to (`AssociatedBundleIdentifiers`), so System
Settings lists it as CC Same rather than under the name on the signing certificate.

It needs no network access. A full pass over three accounts with about 200 sessions each peaks at
roughly 25 MB of memory.

`<state>` is `~/Library/Application Support/cc-same` (macOS), `%APPDATA%\cc-same` (Windows)
or `~/.local/share/cc-same` (Linux).

## The desktop app

The app is a window over the same engine, plus a menu bar (macOS) or notification area (Windows,
Linux) icon. The icon keeps running when the window is closed and shows the headline, *Sync now*
and the background switch; it is a `tray-icon` status item on macOS and Windows and a
StatusNotifierItem (`ksni`) on Linux. With the icon hidden, closing the window quits the app.
*Open at login* registers the app to start with `--hidden`, so it comes up in the tray only.

The app never syncs by itself: it reads, and runs the engine when you press *Sync now*. The
background switch installs the same agent as `cc-same install`.

Its preferences live next to the sync settings in `<state>/config.json`: `appearance`
(`system`, `light`, `dark`), `language` (`system` or a code such as `zh-CN`), `tray`,
`checkUpdates` and `autoUpdate`.

### Updates

The app is the only part that goes online, and only for this. A few seconds after it starts and
then once a day it asks GitHub's API for the latest release. When there is a newer one:

1. It downloads the archive for this system (`CC-Same-<version>-<platform>`) into
   `<state>/updates` and checks its size and SHA-256 against the digest GitHub recorded at upload.
2. It unpacks the archive and starts the new program with `--version`, which must answer with
   the expected version. On macOS the new bundle must also pass `codesign --verify --deep --strict`
   and be signed by the same team as the running copy (a copy without a team, built locally,
   accepts only what Gatekeeper accepts).
3. It swaps the new copy in: on macOS by renaming the bundle, on Windows by renaming the running
   program aside (Windows allows that, not overwriting it), on Linux by replacing
   `bin/cc-same-app`, and `bin/cc-same` next to it. Without permission to write there, it offers
   the download page instead.
4. It starts the new copy with `--after-update <pid>` and quits; the new copy waits for the old
   one to exit, then removes what is left of it.

With *Install updates automatically* on (the default), step 3 waits until the window is closed or
the app quits; otherwise **Update** does it all at once. A development build does not update
itself. The background agent keeps running the version it started with, so after an update the
app sets it up again the next time it starts. *What's new* shows the
release notes, from [CHANGELOG.md](../CHANGELOG.md).

## Transcripts

CC Same copies session records, not transcripts: every account already reads the same
`~/.claude/projects`. Since Claude Code 2.1.248 the transcript of a session started or last
continued in Claude Desktop or Cowork is kept at any age. `desktopSessionCleanupPeriodDays` can give
those an age limit, and a managed `cleanupPeriodDays` applies to them too. `cleanupPeriodDays`
(30 days by default) still covers terminal sessions and other data, and before 2.1.248 it covered
Desktop sessions as well. A session whose transcript is gone still shows in the list but opens empty.

CC Same warns only when Desktop sessions would lose their transcripts within a year.
**Keep them** in the app, or `cc-same retention --keep`, removes the Desktop limit (on an older
Claude Code it raises `cleanupPeriodDays` to ten years instead), backs up the settings file first,
and can be undone with **Undo** or `cc-same retention --undo`. Don't set `cleanupPeriodDays` to
`0`: Claude Code rejects it.

## What cannot be synced from the outside

- **Sidebar groups and order.** Desktop keeps them per account and organization in its web
  storage (`dframe-store`) and syncs them with the server as the user setting `ccd/dframe-store`.
  At every start and account switch the server's copy replaces the local one.
- **Pins.** They are star-synced with each account's server settings, so they stay per account.
- **claude.ai chats, projects, memory, cloud Code and Cowork sessions, connectors, routines.**
  These live in each account on Anthropic's servers.

## Desktop's own importer

Desktop 2.9939 has **Import Claude Code CLI Sessions…** in its menu. It finds sessions from other
organizations too. It is a one-off copy, though: archive state and permission grants are not
restored, and later changes do not follow. CC Same keeps every account identical continuously,
so the importer finds nothing left to import.
