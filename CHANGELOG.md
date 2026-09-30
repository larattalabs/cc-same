# Changelog

What changed in each version of CC Same. The app shows these notes after it updates, and under
Settings › Updates.

## 0.1.7 - 2026-09-30

### Fixed

- **Switch Account was missing from the menu bar.** The menu is built when the app starts, before
  it has read your accounts, and it added Switch Account only if it already knew then that switching
  works here, so it never did. On macOS it is now always there; the accounts fill in once they are
  read.

## 0.1.6 - 2026-09-30

### Changed

- **Switch Account in the menu bar lists every account, and asks first.** It used to list only the
  accounts CC Same had kept a sign-in for, so until your first switch it showed just the account in
  use. Now every account is there. Choosing one asks before Claude restarts, since a menu item is
  easy to hit by mistake; an account whose sign-in isn't kept yet offers to sign in to it instead.

## 0.1.5 - 2026-09-30

### Fixed

- **An account showed up once per organization**, the same email on two rows, and every row said
  “Open in Claude”. Claude keeps a session list for each organization an account is used in, and
  signing in to one account after another can leave an empty list in the other account’s
  organization. The window now shows each account once, reporting the list Claude shows for it,
  and marks only the account Claude is signed in to as open. CC Same still keeps every list the
  same. `cc-same doctor` also names only that account as open.
- **Updates to an account could wait until Claude restarted** when Claude had been open for a few
  days. CC Same read only the end of Claude’s log to see which account is in use, and could fall
  back to an older log’s line about another account. It now reads the whole log.

## 0.1.4 - 2026-09-30

### Fixed

- **Background sync stopped after updating** on macOS, with “launchctl bootstrap failed: 5:
  Input/output error”. After an update the app restarts the background agent: it now restarts it
  in place, waits for launchd to let go of the old one when it has to load it again, and retries.
  An agent that is switched on but not loaded is set up again by itself when the app opens, and a
  failure there no longer shows an error: the background switch says how it stands.

## 0.1.3 - 2026-09-29

### New

- **Switch accounts in one click** (macOS). Each account in the window has **Switch**, and the menu
  bar has **Switch Account**: Claude restarts signed in to the other account, without signing out
  of either. CC Same keeps each account’s sign-in on this Mac, as Claude stored it, and trades them
  while Claude restarts. To add an account, **Sign in** (or **+**) restarts Claude on its sign-in
  page; the account you were using stays one click away. On the command line: `cc-same accounts`,
  `cc-same switch`, `cc-same sign-in` and `cc-same forget`.

### Fixed

- **Check for Updates… in the app menu opened a second window.** It now brings the one window
  forward.
- **Messages shown while Settings is open** were pushed off the left edge of the window. They now
  appear centered at the bottom, over the settings.

## 0.1.2 - 2026-09-29

### Fixed

- **Background sync on macOS never started.** The switch stayed at “starting…” and then said “On,
  but not running”. The app set the agent up as a copy of its own program, and macOS refuses to run
  that program outside the app. The agent now runs the app’s program where it is, and an agent set
  up by 0.1.0 or 0.1.1 is set up again the next time the app opens. Run from a disk image, the app
  first asks to be moved to the Applications folder.
- **System Settings lists the background item as CC Same**, with its icon. 0.1.1 meant to, but
  the copy stood in the way.

## 0.1.1 - 2026-09-29

### New

- **Updates inside the app.** CC Same looks for a new version on GitHub once a day, downloads it in
  the background, checks it, and installs it when its window is closed or it quits. **Update** in
  the window installs it right away. Settings › Updates turns either off and shows what changed in
  each version.
- **Check for Updates…** in the menu bar / tray menu and in the app menu.

### Fixed

- **No false alarm about transcripts.** Since Claude Code 2.1.248, the transcripts of Claude Desktop
  sessions are kept at any age unless a setting limits them. CC Same now reads the settings that
  apply (yours, your organization’s, and which Claude Code you have) and warns only when a
  transcript would really be deleted. **Keep them** can be undone from Settings, and
  `cc-same retention` explains the same, with `--keep` and `--undo`.
- **“On, but not running” right after switching on.** The background switch now says the agent is
  starting, and the agent checks in every 15 seconds, even during a long sync.
- **The background item in System Settings** is listed as CC Same, with its icon, instead of under
  the name on the developer’s certificate. The background agent is updated along with the app.

## 0.1.0 - 2026-09-29

The first release: keep Claude Desktop’s local sessions identical across all your accounts. Sign
out, sign in with another account, and everything is still there.

- Every account's local Code sessions stay in step: titles, archive state, deletions, scheduled
  tasks and task suggestions. Local Cowork sessions are optional (experimental).
- The index Claude Desktop has loaded is never touched. CC Same mirrors it to the other accounts,
  and catches up the one you left after you switch accounts or quit.
- Snapshots before the first change and at most hourly afterwards, restorable from Settings or with
  `cc-same restore`. Anything removed or replaced goes to a trash folder for 30 days.
- A small app with a menu bar / tray icon and an optional quiet start at login, in light and dark,
  and in 10 languages.
- A command-line tool on the same engine: `doctor`, `plan`, `sync`, `install`, `restore` and more.
