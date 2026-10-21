# windows-watch-port: plan for `--watch` and `-b --watch` on Windows

Written by Claude Opus 5.5 on 2026-10-21, at d53a2a2a. This is a plan: nothing in it is implemented, and no watch
has run on Windows. Go reference: the pinned commit `b85298b6` (`Cargo.toml` `[workspace.metadata.typescript]`).

## Where it stands

- `tsrs --watch` and `tsrs -b --watch` exit 5 with "watch mode (--watch) on this platform is not supported by
  tsrs." (`execute.rs:319`, `:629`). Checked 2026-10-21 with a Windows dev build installed from the npm tarballs.
- Everything above the OS layer is ported and platform-independent: the watch loop, `watchmanager`, the debouncer
  and the event list. The `tscWatch` and `tsbuildWatch` scenarios run on the mock backend (39 of 42 and 65 of 65,
  per `docs/COMPATIBILITY.md`).
- The missing piece is the Windows backend in `tsrs_fswatch`: `windowsWatcher` has no factory, so it is never
  available (`watcher.rs:297`, `lib.rs:3`). `default()` already returns it on Windows (`watcher.rs:387`).
- The `tsrs_fswatch` tests do not compile on Windows: `watcher_test.rs:160` calls `std::os::unix::fs::symlink`
  (`cargo test -p tsrs_fswatch` fails with E0433).
- No CI job runs on Windows. `ci.yml` is all Ubuntu; `release.yml` builds the Windows binary and smoke-tests the CLI.

## The Go code to port

| Go file | Lines | Needed | Notes |
| --- | ---: | --- | --- |
| `fswatch/windows.go` | 440 | yes | the backend; see below |
| `fswatch/walkdir_windows.go` | 68 | no | `windows.go` never calls `walkDir` (only inotify, fanotify and kqueue do); port it only with Go's walkdir tests |
| `nativepath/realpath_windows.go` | part of 129 | partly | `physicalDirFor` (`watcher.go:855`) calls `nativepath.Realpath`; see step 2 |

`windows.go` uses no shared event loop. Each directory watch:
- opens the directory with `CreateFile` (`FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED`);
- arms the first overlapped `ReadDirectoryChangesW` in `subscribe`, before returning, so a write right after
  `WatchDirectory` is seen;
- runs a goroutine that waits on the read's event, arms the next read before parsing the finished buffer, and walks
  the `FILE_NOTIFY_INFORMATION` chain:
  - added or renamed to: `create`
  - modified, files only: `update`
  - removed or renamed from: `removeAndGetSequence` plus `terminateCallbacksForDeletedRoot`

Error handling:

| Error | What Go does |
| --- | --- |
| `ERROR_OPERATION_ABORTED` | normal stop |
| `ERROR_INVALID_PARAMETER` | retry with a 64 KB buffer instead of 1 MB (network shares) |
| `ERROR_NOTIFY_ENUM_DIR` | `ErrOverflow` |
| `ERROR_ACCESS_DENIED` | if the root is gone, a terminal "watched directory removed" error; otherwise fatal |

`fatal` hands `handleWatcherError` to another goroutine, because `closeWatch` waits for the run goroutine to finish.
Stopping calls `CancelIoEx`, and the run goroutine closes the handle itself after the cancelled read completes.

## Steps

### 1. Windows path semantics in `watcher.rs` (first: everything else depends on it)

Go's `watcher.go` uses `path/filepath`, which on Windows handles:
- drive letters and UNC volumes;
- both separators, with `Clean` producing `\`;
- `Separator` as `\`.

The Rust port uses `/` throughout:
- the absolute-path checks `dir.starts_with('/')` (`watcher.rs:776`, `:854`) reject every Windows path with
  `errNotAbsolute`;
- `filepath_clean` (`:1711`), `filepath_dir` (`:1741`), `rebase_path` and `join_path_suffix` (`:1627`-`:1660`),
  and the child and in-directory checks (`:1679`-`:1707`) split on `/` only.

What to do:
- Port the Windows behaviour of Go's `filepath.VolumeName`, `IsAbs`, `Clean` and `Dir`, choosing between `/` and
  `\` with `cfg`. Before writing one, check whether `tsrs_core` already has a port.
- Port Go's `filepath` test tables for those functions, as `#[cfg(windows)]` unit tests.
- Find out the path form callers pass in. `tsrs_execute`'s `watchmanager` and `tsrs_lsp` hold tspath paths
  (`c:/x/y`). Go passes the same paths and lets `filepath.Clean` turn them into `\`, and its events come back as
  `dir + "\\" + name`. Find where Go turns event paths back into tspath form (`watchmanager`, `project`) and do
  the same; a path that is never turned back matches no file, and the watch silently does nothing.

### 2. `physical_dir_for` on Windows

`physical_dir_for` (`watcher.rs:1612`) calls `std::fs::canonicalize`. On Windows that returns `\\?\C:\...`, which
never equals `dir`, so every watch would get a physical directory with the verbatim prefix. Event paths would then
be rebased from it, or come back with the prefix still on.

- Use the same conversion as `tsrs_vfs`'s `native_realpath` (`osvfs/os.rs:155`), which strips `\\?\` and turns
  `\\?\UNC\` into `\\`. Move it somewhere both crates can call rather than copying it.
- Compare with `nativepath.Realpath`, which follows libuv's `GetFinalPathNameByHandle`, on two points:
  - the case of the drive letter;
  - mapped network drives, which may resolve to their UNC path.

### 3. The backend: `crates/tsrs_fswatch/src/windows.rs`

Estimate: about 500 lines.
- Gate the module with `#[cfg(windows)]`.
- Install the factory in `windowsWatcher` the way `inotify_linux::init` is installed (`watcher.rs:273`).
- Keep Go's names and order, with a `// windows.go:<line>` comment above each function (`docs/PORTING.md`).

How the pieces map:
- **Goroutine per watch** becomes a named `std::thread` per watch. The CLI makes few watches (recursive watches,
  consolidated), so a thread each is cheap. Moving to an I/O completion port would need numbers.
- **Cancellation.** Go starts a helper goroutine per read to call `CancelIoEx` when `stopCh` closes. The Rust
  version should wait on `[read event, stop event]` with `WaitForMultipleObjects` and cancel from the run thread:
  the same behaviour, without a thread per read. This differs from Go, so it needs a comment saying why.
- **Buffer lifetime.** This is the main correctness risk. The kernel writes into the buffer and the `OVERLAPPED`
  until the read completes or the cancelled read is reaped, and Go relies on its garbage collector for that. In
  Rust:
  - keep each read in a `Box`, so it never moves;
  - free it only after `GetOverlappedResult` has returned for that read, including after `CancelIoEx`;
  - give every `unsafe` block a `SAFETY` comment (`docs/RUST.md`).
- **Alignment.** `FILE_NOTIFY_INFORMATION` entries are DWORD-aligned. Back the buffer with `Vec<u32>`, not
  `Vec<u8>`.
- **`fatal`.** Spawn a thread to call `handle_watcher_error`, as Go does. Calling it from the run thread
  deadlocks in `close_watch`.
- **Dependencies.** Add a `[target.'cfg(windows)'.dependencies]` entry for `windows-sys` with the features for
  `Foundation`, `Storage::FileSystem`, `System::IO` and `System::Threading`. `Cargo.lock` already has 0.52,
  0.59 and 0.61 as indirect dependencies; use 0.61 so no new version is added.

Code that becomes live on Windows (from searching the crate for "windows"):
- `event.rs`: `create`, `update`, `remove`, `set_error`, `create_locked`, `update_locked` and `is_deleted` are
  `#[cfg(any(unix, test))]` (`:70`-`:294`), and `remove_and_get_sequence` has an `#[expect(dead_code)]` naming
  `windows.go` (`:113`).
- `debounce.rs:93`: `trigger` is `#[cfg(unix)]`.
- `watcher.rs`:
  - `err_watched_directory_removed` is `#[cfg(unix)]` (`:137`);
  - `handle_watcher_error` and `dirWatchError::dir_watch` have `expect(dead_code)` naming `windows.go` (`:1053`,
    `:1092`);
  - `dirWatch::state` is macOS-only (`:1122`);
  - `terminate_callbacks_for_deleted_root` and its callback fields are macOS-only (`:1083`, `:1222`).

Leave `can_share_recursive_dir_watches` (`watcher.rs:522`) as Go has it: FSEvents only, with an upstream TODO
about Windows.

### 4. Write settling (tsrs-only)

The CLI turns on write settling (`set_write_settling`, `watcher.rs:351`): callbacks wait until no file is being
written.
- inotify knows this from `IN_CLOSE_WRITE`.
- FSEvents and kqueue have no close event, so a file that is empty when its event arrives counts as still being
  written (`notes/watch-macos.md`).
- `ReadDirectoryChangesW` has no close event either.

The options:

| Option | How it works | Trade-off |
| --- | --- | --- |
| A. Go's behaviour | fire on the first event after a quiet period | a truncate-then-write save runs 2 cycles, the first on an empty file |
| B. the macOS rule | an empty file holds the callbacks | shared code, already tested |
| C. a sharing probe (Windows only) | on a modified event, open the file without `FILE_SHARE_WRITE`; `ERROR_SHARING_VIOLATION` means another handle has write access (`write_started`); probe again until the open succeeds (`write_finished`) | antivirus, the search indexer and editors that keep a handle open can hold the callbacks, though `maxWaitTime` and the long-lived set already cap that; one extra open per modified file |

Recommendation: ship B with the backend. Then run `savepatterns.py` on Windows, and try C only if common editors
still produce two cycles per save. Record the results and the rejected options in `notes/watch-windows.md`, as
`notes/watch-macos.md` does for macOS.

### 5. The language server

Today, a client that cannot register file watches makes `tsrs_lsp` fall back to the in-process watcher whenever
`has_fast_recursive_backend()` is true (`server.rs:1937`). That function checks only the backend's name
(`watcher.rs:725`, `"windows" | "fsevents"`), not whether the backend is available. So on Windows today the
fallback is chosen and every watch then fails with `ErrUnavailable`.
- This port fixes that case.
- Before the port, write a test showing the current behaviour, and check whether Go also checks only the name.

## Tests (each seen red first)

1. **Make the suite compile on Windows.** Replace the symlink helper with a directory junction (`mklink /J`), as
   Go's `makeDirSymlink` does (`watcher_test.go:123`). With the stub backend, every test that creates a watch then
   fails with `ErrUnavailable`: that failure is the red for the backend.
2. **Port the missing Go tests that exercise the backend.** 33 of Go's 87 `watcher_test.go` tests are ported. The
   ones the Windows backend needs:
   - rename pairs;
   - removing the watched root;
   - an overflow;
   - many changes in one buffer;
   - a write immediately after subscribing.

   Go skips two `chmod` tests on Windows (`:2579`, `:3057`); skip them too. The 64 KB network-share fallback cannot
   be tested locally; call it unvalidated.
3. **End to end.** Add Windows counterparts of `tests/write_settling.rs` and `tests/write_settling_fsevents.rs` for
   the option chosen in step 4. Make the `--watch` and `-b --watch` cases in `watch_macos.rs` run on Windows, or
   copy them.
4. **The watch tools** (`watchcheck.py`, `editduringjob.py`, `savepatterns.py`): check that they run on Windows,
   where paths differ and Ctrl-C is `CTRL_C_EVENT`, then run them on a real project.
5. **Regression.** The mock-backed `tscWatch` and `tsbuildWatch` scenarios must still pass 39 of 42 and 65 of 65.

## CI

Add a `windows-latest` job that runs `cargo test -p tsrs_fswatch` and `tsrs_execute`'s watch tests. Without it,
nothing exercises the backend, and the macOS note shows how much shared watcher code changes. Also consider a
`--watch` smoke test in `release.yml`'s Windows job: start the watch, edit a file, check the cycle, stop it.

## Pull requests

1. **Paths and tests.** Steps 1 and 2, the test suite compiling on Windows, and the Windows CI job running the
   pure-function tests.
2. **The backend.** Step 3: the `cfg` widening, `windows-sys`, the Go tests from the list above, write-settling
   option B, and the end-to-end tests. Remove the "not supported" exits in `execute.rs`. Update the docs below.
3. **Settling, if needed.** Measurements on Windows, option C if they call for it, and `notes/watch-windows.md`.

Docs to update in pull request 2 (`AGENTS.md`: keep the capability table current):
- `docs/COMPATIBILITY.md`: the `--watch` row (`:58`).
- `docs/PORTING.md:217`: "the fanotify and Windows file-watching backends".
- The header comments in `tsrs_fswatch` (`lib.rs:1`-`4`, `watcher.rs:297`).
- The root and npm README compatibility summaries, if they mention Windows watching.

## Size and risks

Roughly:

| Work | Rust lines |
| --- | ---: |
| backend | 500 |
| path semantics | 150-250 |
| realpath and `cfg` changes | 50 |
| tests | 300 |
| CI job | about 30 lines of YAML |

Medium overall. The risks, largest first:
- **Unix assumptions outside `tsrs_fswatch`.** Steps 1 and 2 may turn up `/`-only path code in `watchmanager` or
  where events are matched to program files.
- **Overlapped I/O lifetimes** in `unsafe` code (step 3).
- **Write settling** (step 4).
- **Case-insensitive NTFS.** Go compares paths exactly on Windows (`canonicalize_other.go`). The macOS port needed
  fixes for case-only renames (`notes/watch-macos.md`), so check that events match program paths when the case of
  the drive letter or a directory differs from the program's spelling.

## Implementation report

Written by Claude Sonnet 5.5 (`claude-sonnet-5-5`), 2026-10-21, on branch `watch-windows`, from d53a2a2a. The plan above
was written by Opus 5.5. **Nothing is committed**: every change below is in the working tree. All numbers are from this
machine (Windows 11, dev-profile builds, Rust 1.99.1) unless stated.

### Result

`tsrs --watch` and `tsrs -b --watch` (the `-b` path shares the watch manager, but only `--watch` was run by hand) now
run on Windows instead of exiting with status 5. A watch on a small project started exactly one rebuild, with the right
diagnostics, for each of: an edit that introduces an error, the fix, a new file with an error, deleting it, a file in a
**new subdirectory**, renaming it, and removing the directory. The process was stopped and checked gone after each
session. The plan's steps 1-4 and the CI job are done; step 5 (language server) needed no change beyond the backend.

### What was built

| Change | Files |
| --- | --- |
| Go's `filepath` (`Clean`, `Dir`, `IsAbs`, `VolumeName`) for Windows and Unix rules, ported from Go 1.27's `internal/filepathlite` including its lazy buffer and `postClean`. `Os::{Unix, Windows}` plus `HOST` (the build's own) so both flavours are testable on any OS | `tsrs_fswatch/src/filepath.rs` (320 lines), `filepath_test.rs` (160) |
| Go's `nativepath.Realpath`: `canonicalize` returns `\\?\C:\x`, which is stripped (`\\?\UNC\h\s` becomes `\\h\s`; other verbatim forms stay) | `nativepath.rs` (40) |
| `watcher.rs` no longer assumes `/`: absolute-path check, `Dir`/`Clean`, `rebase_path`, `join_path_suffix`, `is_in_directory_or_self`, `is_direct_child`, `child_on_path`, `physical_dir_for` | `watcher.rs` |
| The backend: overlapped `ReadDirectoryChangesW`, one thread per watch | `windows.rs` (506), `Cargo.toml` (`windows-sys` 0.61, Windows only; already in `Cargo.lock` indirectly, +1 lock line) |
| Shared code made available on Windows: cfg gates widened from `unix`/`macos` to include `windows` (`event.rs`, `debounce.rs`, `watcher.rs`) | |
| Write settling on Windows (an empty file holds the rebuild) | `windows.rs` `settle`, `tests/write_settling_windows.rs` (136) |
| Event paths converted to `/` where the watch manager records them (Windows only) | `tsrs_execute/src/watchmanager/mod.rs` `slash_path` |
| A Windows `pipe()` (two in-memory byte queues) so `tsrs_execute`'s tests compile on Windows | `tsrs_ipc/src/stream.rs` |
| Windows CI job `test-windows-watch` | `.github/workflows/ci.yml` |
| Docs | `COMPATIBILITY.md` (the `--watch` row), `DEFAULTS.md`, `LSP.md`, `PORTING.md` |
| Two unrelated Windows-only Clippy errors that blocked linting the dependent crates there | `tsrs_vfs/src/osvfs/os.rs:159`, `tsrs_execute/src/tsc/statistics.rs:116` |

The diff is 17 files, +1,341/-144, of which `watcher_test.rs` is +1,058.

### How it differs from the plan

- **Paths: flavour enum, not `cfg` alone.** The plan said to port Go's Windows `filepath` behind `cfg`. `Os` takes the
  flavour as a value, so the Windows rules run in unit tests on Linux CI too. The 63 Windows rows in `filepath_test.rs`
  are Go 1.27.1's own answers, produced by running Go on this machine (`filepath.Clean`/`Dir`/`VolumeName`/`IsAbs`).
  The helper expectations in `watcher_test.rs` (`rebase_path`, `join_path_suffix`, `is_in_directory_or_self`,
  `is_direct_child`) were checked the same way against Go's unexported functions copied into a scratch program.
- **Per-watch state lives in the backend** (`FxHashMap<dwKey, Arc<windowsSubscription>>`), not in `dirWatch.state`,
  which stays macOS-only.
- **The first read is armed on the watch's own thread**, which answers a channel before `subscribe` returns. Go arms it
  on the caller's goroutine. This keeps the guarantee (a change made after `subscribe` returns is seen) without
  moving an `OVERLAPPED` between threads, so there is no `unsafe impl Send`.
- **Cancellation waits on two events** (the read's and a stop event) instead of Go's helper goroutine per read.
  `windowsRead::drop` cancels and reaps a read still pending, because the kernel writes into its buffer until then.
- **Zero-byte completion is an overflow.** A read that completes successfully with no bytes means the kernel dropped
  changes; Go parses the empty buffer as no changes. This is tsrs-only, and it is not covered by a test.
- **Event paths are converted to `/` in the watch manager.** Go's CLI passes the raw `C:\proj\a.ts` event path on to
  `possibly_matches_file_name`, whose globs use `/`; on Windows that made a newly created file "not relevant to
  compilation" and no rebuild ran (seen in the debug log). The language server's watcher already normalizes
  (`lspwatcher.rs:298`). tsgo's CLI may have the same problem on Windows; that is unchecked.
- **Write settling: the plan's option B** (an empty file holds callbacks), as on macOS. Option C (a sharing-violation
  probe) was not tried. The idle and maximum hold (1 s, 2 s) are the shared constants. A removed file also releases
  its hold, which macOS does not do.
- **The `tsrs_ipc` change was not in the plan.** `tsrs_execute`'s tests could not compile on Windows at all (its
  dev-dependency `tsrs_contentmappertest` needs `ipc::pipe()`, which existed only for Unix), so neither the plan's CI
  line `cargo test -p tsrs_execute watch` nor a watch-manager test could exist. A loopback-TCP version was tried first
  and failed 10 of the 31 `tsrs_ipc` tests (closing one end does not wake a read blocked on a duplicated socket
  handle); the in-memory queues pass all 31. The `tsrs_ipc` tests had never run on Windows before.

### Evidence

Tests added or made to run on Windows: `tsrs_fswatch` 93 (51 passed before the backend existed, but most
`run_for_each_watcher` tests ran zero watchers), `tsrs_ipc` 31, `tsrs_execute` 12, plus `write_settling_windows` (1,
repeated 6 times). About 40 Go watcher tests were ported in three batches (renames, recursive create and delete,
coalescing, truncate and append, replace dir/file, round trip, nested delete, non-recursive and file watches,
junction rebasing, batched `WatchDirectories`, error forwarding). Flakiness: 8 consecutive runs of the 51-test suite
and about 5 of the 93-test suite had no test retries (the harness retries a failing body up to 3 times and prints it). Where a Rust test hardcoded `/`, it now uses `join`/`native` helpers (Go uses `filepath.Join`).

Red-first was seen for: the Windows path rows, the `rebase`/`join`/`is_in_directory_or_self` helpers, `physical_dir_for`
(by breaking the `\\?\` stripping), the backend (`default_backend_matches_platform`), settling ("fired while the file
was empty"), and `slash_path` (stored `C:\proj\src\c.ts`). Breaking `FILE_ACTION_MODIFIED` handling failed only
`watch_file_update`, so pure modify events are thinly covered.

Save patterns, rebuild cycles per pattern (a rebuild that reports an error for a half-written file is the defect):

| Pattern | Before settling | After |
| --- | --- | --- |
| in-place write, atomic save, rename away + write + delete | 1 | 1 |
| truncate, 200 ms, write | 2 (first reports a spurious error) | 1 |
| write in 4 chunks 100 ms apart | 3 (two spurious errors) | 1 |
| edit outside `include` | 0 | 0 |
| 50 files created, then deleted | 3 | 3, one of which can say `TS6053 file not found` |

The `TS6053` cycle happens when a rebuild reads the directory while files are still being deleted; the next cycle
is clean. macOS cut bursts down with a hold on batches that touch several files; that is not ported here.

Linters: `cargo clippy ... --config 'build.warnings="deny"'` is clean for `tsrs_fswatch`, `tsrs_ipc`, `tsrs_execute`
and `tsrs_vfs` on `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin` (the last two
type-checked only; I installed those two rustup targets for this and deleted their `target/` output).
`cargo check --workspace --all-targets` passes on Windows with only the two `tsrs_testrunner` warnings that were
already there.

### Issues and things someone should know

1. **Not run on GitHub.** The new CI job is validated as YAML and mirrors the release workflow's Windows setup, but it
   has not run. GitHub's Windows runners use 8.3 short temp paths (`C:\Users\RUNNER~1\...`); the tests use
   `new_tmp_dir` (expanded) for expectations, but that is the most likely first failure.
2. **`tools/lint/source.py` fails on this Windows checkout** with 137 lines of "weakened atomic orderings without a
   comment" in files I did not touch, identical with my changes stashed. It names none of my files. CI runs it on
   Linux, where I could not run it.
3. **Not re-run:** the `tscWatch`/`tsbuildWatch` scenario baselines (they need `tools/oracle/tsctests/dump.sh` and a
   patched Go checkout), the conformance and fourslash gates, `-b --watch` by hand, release or PGO builds, and any
   performance measurement (AGENTS.md's bar is for performance changes; this is not one).
4. **The language server on Windows is untested.** It already chose the builtin watcher on Windows, because
   `has_fast_recursive_backend()` tests the backend's name (`server.rs:1937`), but every watch then failed as
   unavailable; they should work now. I only read that path (its watcher already converts event paths to `/`).
5. **Go tests not ported:** the goroutine-leak test (Go-specific; a thread-leak check would be the Rust equivalent), the
   backend-start-error and `dirWatchError` tests, the content-mapper and consolidation tests (Go enables recursive
   consolidation only for FSEvents), the descendant-symlink test (skipped for fast recursive backends) and the two
   denied-subdirectory tests (Go skips them on Windows). The file-symlink tests skip themselves when the platform
   refuses to create a symlink; they ran here.
6. **Untested paths in `windows.rs`:** the 64 KB network-share fallback, `ERROR_NOTIFY_ENUM_DIR`, the zero-byte
   overflow, and the `ERROR_ACCESS_DENIED` root-deleted branch (`subscribe_watched_dir_deleted` passes, but I did not
   confirm which branch it takes). A 1 MB buffer per watch is allocated twice (double buffering).
7. **Unsafe:** 9 `unsafe` uses in `windows.rs`, each with a `SAFETY` comment (event handles, the overlapped read and its
   reaping, an aligned `u32` to `u8` view). No `unsafe impl Send/Sync` was added, so `send_sync.tsv` is unaffected.
8. **Behaviour carried over from Go:** event paths for a root like `C:\` come out as `C:\\name` (Go builds them the
   same way); the watch manager refuses to watch roots anyway.
9. **Case:** nothing here tests a case-only rename or an event whose drive-letter case differs from the program's, the
   risk listed in the plan.
10. **`ipc::pipe()` semantics on Windows** follow its doc comment (close wakes both ends' readers; a write on the peer
    fails). Content-mapper harness tests that use it were not run on Windows.
11. **Docs:** the `--watch` row now says Windows works, with this evidence and "no Windows CI job runs them yet". Update
    it once the job has run.

### Next steps, in order

1. Run the CI job; fix runner-specific failures (item 1). Then drop "no Windows CI job runs them yet" from the table.
2. Run the language server with the builtin watcher on Windows (item 4).
3. Port the macOS burst hold if burst behaviour matters (a formatter or `git checkout` on a large tree).
4. Re-run the scenario baselines and `-b --watch` on a project with references.
5. Commit in the three pull requests the plan proposes, or as one: the diff is mostly tests.

## Review

Written by Claude Fable 5.1 (`claude-fable-5-1`), 2026-10-21, reviewing the uncommitted working tree described in the
implementation report above (same branch, same machine). I read every changed file and the whole of `windows.rs`,
`filepath.rs`, `nativepath.rs` and the `tsrs_ipc` pipe, re-ran the three crates' test suites (12 + 93 + 1 + 31 pass,
as reported), and ran the dev `tsrs.exe` for the two things the report left open.

### Verdict

Sound and faithful to `windows.go`; land it. The backend's `unsafe` is correct by construction, the deviations from Go
are improvements and are documented, and the one bug the port exposed outside `tsrs_fswatch` (event paths with `\`
never matching the config's globs) is fixed at the right boundary with a test. Items 1-3 below are worth doing before
the pull request; the rest are notes for whoever touches this next.

### Verified by running the exe (closes two report items)

- **`-b --watch` works** (report: not run by hand): an edit that breaks `a.ts` and its fix each started one build
  with the right diagnostic. The `COMPATIBILITY.md` row's claim for `-b --watch` is now backed.
- **Case-only rename** (plan risk "Case-insensitive NTFS", report item 9): `b.ts` renamed to `B.ts` gave one rebuild
  reporting TS1149 ("differs from already included file name only in casing"), the rename back gave one clean
  rebuild, and an edit afterwards one more. No stale state; this is the behaviour the macOS note had to fix by hand.

### Correctness of the backend (`windows.rs`)

I traced the overlapped-I/O lifetimes and found no hole:

- A read is a `Box<windowsRead>` created and consumed on the watch's own thread and never sent anywhere, so the
  `OVERLAPPED` (which holds raw pointers and is `!Send`) needs no `unsafe impl Send`; the compiler enforces that it
  stays on that thread. `windowsSubscription` is `Send + Sync` by its field types.
- Reads are reaped before the directory handle closes: in `run`, `current` (and `next`) are declared after `dir`, so
  they drop first, and `windowsRead::drop` cancels and waits for a still-pending read. The read's event handle is
  closed after the reap (struct fields drop after the `Drop` impl).
- The wait-on-two-events design is equivalent to Go's helper goroutine. If the read completes at the same time as the
  stop, `WaitForMultipleObjects` reports the read (lower index) and the loop exits on `stopped`; if the stop wins,
  `CancelIoEx` on an already-completed read is harmless and `GetOverlappedResult(wait=TRUE)` returns the real result.
- No deadlock between `close_watch` (joins the thread) and the thread: the run thread never takes
  `watcherBase.mu`, and `fatal` hands `handle_watcher_error` to another thread before returning, as Go does.
- The zero-byte completion is the kernel's documented "buffer overflowed, enumerate the directory" signal; mapping it
  to `ErrOverflow` is right. Go's walk of its fresh zeroed buffer finds one empty record and reports nothing, so the
  `DEFAULTS.md` wording ("tsgo reads it as no changes") is accurate.
- `FILE_NOTIFY_INFORMATION` is parsed through byte reads (`word(at)`), so entry alignment never matters; a bad
  `FileNameLength` ends the walk instead of reading out of bounds.

### Should fix before the pull request

Items 1 and 2 were applied after this review was written (same session, same author): `win32_error` puts the
system text and the number into every fatal message, `wait` reads `GetLastError` straight after
`GetOverlappedResult`, the `fatal` thread is named, and the three skips print `SKIPPED`. Item 3 is left as it is: a
timing threshold is only worth loosening against an observed flake. `tsrs_fswatch` still passes 93 + 1 with Clippy
clean on Windows and the Linux type-check clean.

1. **Win32 error codes are lost.** `process_completion`'s default branch and the `ERROR_ACCESS_DENIED` fallthrough
   call `fatal(Error::new("unknown error"))`, and the wait failure says `"GetOverlappedResult failed"`. The user sees
   `Warning: File watch error: ... unknown error` with nothing to search for. Go is equally opaque, but there is no
   reason to copy that: include the code (`unknown error: Win32 error {code}`) and, in `wait`, read `GetLastError`
   into a local straight after `GetOverlappedResult` rather than later in the struct literal, where a future edit
   that inserts a call would silently clobber it.
2. **Silent skips in the symlink tests.** `subscribe_symlink_create` and `subscribe_symlink_delete` return `Ok(())`
   when `make_file_symlink` fails. On a GitHub runner without developer mode that is every run, and the tests
   "pass". Print the skip at least (`eprintln!`), as the harness has no skip state; the same for the kqueue early
   return in `round_trip_rename`. They ran for real here (this machine allows symlinks).
3. **CI flake risk in `write_settling_windows.rs`.** `a_complete_save_is_not_delayed` asserts the callback within
   150 ms of the write, on a shared runner, in a dev-profile binary. The macOS twin uses 100 ms and has flaked
   before (it is in its own binary for that reason). Expect to loosen it or retry it after the first CI run.

### Notes, not blockers

- **`settle` releases the hold on the first non-empty `MODIFIED`.** A writer that writes a file in pieces and whose
  pieces each produce a size or last-write notification will be read half-written, exactly as a half-written save is
  on FSEvents. The report's "write in 4 chunks: 3 cycles before, 1 after" is real but incidental: NTFS updates the
  directory entry's size lazily, so for a handle held open the notifications tend to arrive at close. Do not read
  that row as "chunked writers are handled".
- **Memory and churn.** Each watch allocates two 1 MB buffers (one armed while the other is parsed) plus a thread, as
  in Go. The CLI run here made 8 watches, 7 of them non-recursive ancestor watches on `%TEMP%` and above, which
  deliver every temp-file create and delete to the debouncer ("not relevant to compilation" in the debug log). Same
  cost on Linux; worth remembering if the language server registers many roots.
- **`ERROR_OPERATION_ABORTED` while not stopped** ends the thread without an error to the callback and leaves the
  subscription registered until the user closes it. Go does the same; nothing in tsrs cancels I/O on that handle, so
  it should not happen.
- **`lpBytesReturned` is passed as null** to the asynchronous `ReadDirectoryChangesW`. The docs call the value
  undefined for overlapped calls and the parameter optional; Go passes a pointer. Either is fine.
- **`nativepath::realpath` duplicates `tsrs_vfs::osvfs::native_realpath`** (the plan asked for one copy). A shared home
  would be `tsrs_core`, since `tsrs_fswatch` cannot depend on `tsrs_vfs`; a follow-up.
- **Names:** `Event(usize)` in `windows.rs` is a Win32 event object, next to `crate::event::Event` the filesystem
  event; `Win32Event` would read better. `fatal` spawns an unnamed thread where the crate names its threads.
- **`filepath.rs`** reproduces `filepathlite` including the `lazybuf` quirks (`postClean` scans the whole buffer, as
  Go does). The 63 oracle rows cover the drive, UNC, device-prefix and `postClean` branches; I found no divergence by
  reading. The `Os` flavour enum is a good call: the Windows rules are unit-tested on Linux CI too.
- **`slash_path` in the watch manager** is the right boundary: fswatch keeps Go's native spelling (its tests expect
  it), and everything above the manager is `/`. The language server's watcher already normalized
  (`lspwatcher.rs:298`) and never compares event paths with its roots, so it needed nothing.
- **`tsrs_ipc` Windows `pipe()`**: a plain condition-variable byte queue; the semantics in its comment match what the
  31 tests check. Unbounded, which Go's `net.Pipe` is not (it is synchronous), but nothing in tsrs relies on
  back-pressure from it.
- **`tools/lint/source.py` on Windows** reports "baseline 0" for files that certainly have baselines on Linux, so the
  likely cause is `\` paths from the directory walk not matching `/` baseline keys. Unchecked; not part of this change.
- **Go test coverage:** 64 of Go's 87 `watcher_test.go` tests now have a Rust counterpart (the report's "about 40
  ported" is the number added this round). The 23 without one are listed in the report's item 5 and are the right
  ones to leave.

### Docs

The capability table, `DEFAULTS.md`, `LSP.md` and `PORTING.md` say what the code does and no more; "no Windows CI job
runs them yet" is the one line to retire after the first green run. With the `-b --watch` run above, the row's claim
for both commands is backed by a hand run; the scenario baselines remain unmeasured on Windows, as the row says.
