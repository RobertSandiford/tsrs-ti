// windows.go: ReadDirectoryChangesW backend (Windows).
//
// Overlapped (asynchronous) ReadDirectoryChangesW on one directory handle per watch. There is no shared event loop:
// each watch owns a thread (Go: a goroutine) that waits for its read to complete, arms the next read before it
// parses the finished buffer, and turns the FILE_NOTIFY_INFORMATION records into events.
//
// Go starts a helper goroutine per read that cancels it when the watch stops; here the wait is on two events (the
// read's and the watch's stop event) and the waiting thread cancels. A read that is still pending when its thread
// leaves is cancelled and reaped by `windowsRead::drop`, because the kernel writes into its buffer until then.
//
// tsrs-only: a read that completes with zero bytes means the kernel dropped changes (the buffer overflowed); Go reads
// the empty buffer as no changes at all.

use rustc_hash::FxHashMap;
use std::fs::OpenOptions;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NOTIFY_ENUM_DIR,
    ERROR_OPERATION_ABORTED, FALSE, HANDLE, TRUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects,
};

use crate::watcher::{
    ErrOverflow, Error, dirWatch, dirWatchError, dwKey, err_watched_directory_removed,
    handle_watcher_error, watcher, watcherBase, watcherImpl, write_settling,
};

const defaultBufSize: usize = 1024 * 1024;
const networkBufSize: usize = 64 * 1024;

const notifyChangeFilter: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE;

// windows.go:117 (init)
pub(crate) fn init(mut w: watcher) -> watcher {
    w.factory = Some(|| Arc::new_cyclic(|me| new_windows_backend(Weak::clone(me))) as Arc<dyn watcherImpl>);
    w
}

pub(crate) struct windowsBackend {
    base: watcherBase,
    // `fatal` needs the backend as a `watcherImpl` from a subscription's thread.
    me: Weak<windowsBackend>,
    // Go keeps this in `dirWatch.state`.
    subscriptions: Mutex<FxHashMap<dwKey, Arc<windowsSubscription>>>,
}

// windows.go:121
fn new_windows_backend(me: Weak<windowsBackend>) -> windowsBackend {
    windowsBackend { base: watcherBase::default(), me, subscriptions: Mutex::new(FxHashMap::default()) }
}

fn last_os_error_code() -> u32 {
    std::io::Error::last_os_error().raw_os_error().map_or(0, i32::cast_unsigned)
}

// Go's errUnknown names no code; the system's text and the number are what a report needs.
fn win32_error(code: u32) -> String {
    let err = std::io::Error::from_raw_os_error(code.cast_signed());
    format!("{err} (Win32 error {code})")
}

// A Win32 event object. The handle is held as an integer so that the type is `Send` and `Sync`.
struct Event(usize);

impl Event {
    fn manual_reset() -> Result<Event, Error> {
        // SAFETY: no security attributes and no name; the result is checked.
        let handle = unsafe { CreateEventW(std::ptr::null(), TRUE, FALSE, std::ptr::null()) };
        if handle.is_null() {
            return Err(Error::new(format!("CreateEvent: {}", std::io::Error::last_os_error())));
        }
        Ok(Event(handle as usize))
    }

    fn handle(&self) -> HANDLE {
        self.0 as HANDLE
    }

    fn set(&self) {
        // SAFETY: `self` owns a live event handle.
        unsafe { SetEvent(self.handle()) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: `self` owns the handle and nothing uses it afterwards.
        unsafe { CloseHandle(self.handle()) };
    }
}

// windows.go:155. One armed ReadDirectoryChangesW: the kernel writes into `buf` and `overlapped`, so a read stays at
// one address (it lives in a `Box`) and is reaped before it is freed.
struct windowsRead {
    // u32 elements: the kernel requires a DWORD-aligned buffer.
    buf: Vec<u32>,
    overlapped: OVERLAPPED,
    event: Event,
    // The directory handle the read is pending on.
    dir: usize,
    pending: bool,
}

// What `windowsRead::wait` found.
struct completion {
    bytes: usize,
    // Whether the wait itself worked.
    waited: bool,
    // The Win32 error of the read, if it failed.
    err: Option<u32>,
}

impl windowsRead {
    // windows.go:224 (wait)
    fn wait(&mut self, stop: &Event) -> completion {
        let handles = [self.event.handle(), stop.handle()];
        // SAFETY: `handles` holds two live event handles.
        let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), FALSE, INFINITE) };
        let completed = waited == WAIT_OBJECT_0;
        if !completed {
            // SAFETY: the read was armed on `dir` with this `overlapped`.
            unsafe { CancelIoEx(self.dir as HANDLE, &raw const self.overlapped) };
        }
        let mut bytes = 0u32;
        // SAFETY: as above; after a cancel this waits for the read to finish, so the buffer is no longer in use.
        let ok = unsafe {
            GetOverlappedResult(
                self.dir as HANDLE,
                &raw const self.overlapped,
                &raw mut bytes,
                if completed { FALSE } else { TRUE },
            )
        };
        // Read before anything else can call into the OS and replace it.
        let err = (ok == FALSE).then(last_os_error_code);
        self.pending = false;
        completion { bytes: bytes as usize, waited: completed || waited == WAIT_OBJECT_0 + 1, err }
    }
}

impl Drop for windowsRead {
    fn drop(&mut self) {
        if !self.pending {
            return;
        }
        let mut bytes = 0u32;
        // SAFETY: the read is still armed on `dir` with this `overlapped`; cancelling and waiting for it is the only
        // way to know the kernel has finished with `buf`.
        unsafe {
            CancelIoEx(self.dir as HANDLE, &raw const self.overlapped);
            GetOverlappedResult(self.dir as HANDLE, &raw const self.overlapped, &raw mut bytes, TRUE);
        }
    }
}

struct subscriptionState {
    stopped: bool,
    buf_bytes: usize,
}

// windows.go:131
struct windowsSubscription {
    backend: Weak<windowsBackend>,
    dir_watch: Arc<dirWatch>,
    mu: Mutex<subscriptionState>,
    stop_event: Event,
    // Go's `doneCh`: joined by `close_watch`.
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl windowsSubscription {
    // windows.go:141 (newWindowsSubscription), without the directory handle: the thread that reads owns it.
    fn new(backend: Weak<windowsBackend>, w: &Arc<dirWatch>) -> Result<windowsSubscription, Error> {
        Ok(windowsSubscription {
            backend,
            dir_watch: Arc::clone(w),
            mu: Mutex::new(subscriptionState { stopped: false, buf_bytes: defaultBufSize }),
            stop_event: Event::manual_reset()?,
            thread: Mutex::new(None),
        })
    }

    // windows.go:141
    fn open_directory(&self) -> Result<std::fs::File, Error> {
        let dir = OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED)
            .open(&self.dir_watch.physical_dir)
            .map_err(|err| match err.raw_os_error() {
                Some(code) => Error::wrap(format!("invalid handle: {err}"), &Error::from_errno(code)),
                None => Error::new(format!("invalid handle: {err}")),
            })?;
        let is_dir = dir.metadata().is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0);
        if !is_dir {
            return Err(Error::new(format!("{}: not a directory", self.dir_watch.physical_dir)));
        }
        Ok(dir)
    }

    // windows.go:179
    fn begin_read(&self, dir: HANDLE) -> Result<Option<Box<windowsRead>>, Error> {
        let buf_bytes = {
            let st = self.mu.lock().unwrap();
            if st.stopped {
                return Ok(None);
            }
            st.buf_bytes
        };
        let event = Event::manual_reset()?;
        let mut req = Box::new(windowsRead {
            buf: vec![0; buf_bytes / 4],
            overlapped: OVERLAPPED::default(),
            event,
            dir: dir as usize,
            pending: false,
        });
        req.overlapped.hEvent = req.event.handle();
        // SAFETY: `req.buf` and `req.overlapped` stay at their addresses (boxed) until the read is reaped, by `wait`
        // or by `windowsRead::drop`; `dir` is open for the lifetime of the read (the thread drops its reads first).
        let ok = unsafe {
            ReadDirectoryChangesW(
                dir,
                req.buf.as_mut_ptr().cast(),
                u32::try_from(buf_bytes).unwrap_or(u32::MAX),
                i32::from(self.dir_watch.recursive),
                notifyChangeFilter,
                std::ptr::null_mut(),
                &raw mut req.overlapped,
                None,
            )
        };
        if ok == FALSE {
            return Err(Error::new(format!(
                "failed to read changes: {}",
                std::io::Error::last_os_error()
            )));
        }
        req.pending = true;
        Ok(Some(req))
    }

    // windows.go:244. The watch's thread: opens the directory and arms the first read before `ready` is answered,
    // so that a change made after `subscribe` returns is seen, then loops on the reads until the watch stops.
    fn run(&self, ready: &mpsc::Sender<Result<(), Error>>) {
        let dir = match self.open_directory() {
            Ok(dir) => dir,
            Err(err) => {
                let _ = ready.send(Err(err));
                return;
            }
        };
        let handle = dir.as_raw_handle() as HANDLE;
        let first = match self.begin_read(handle) {
            Ok(Some(first)) => first,
            Ok(None) => {
                let _ = ready.send(Err(Error::new("fswatch: windows: stopped before the first read")));
                return;
            }
            Err(err) => {
                let _ = ready.send(Err(err));
                return;
            }
        };
        let _ = ready.send(Ok(()));

        let mut current = first;
        loop {
            let done = current.wait(&self.stop_event);
            if !done.waited && let Some(code) = done.err {
                self.fatal(Error::new(format!("GetOverlappedResult failed: {}", win32_error(code))));
                return;
            }
            if self.mu.lock().unwrap().stopped {
                return;
            }
            if done.err.is_some() {
                if self.process_completion(done.err, &current.buf, done.bytes) {
                    return;
                }
                match self.begin_read(handle) {
                    Ok(Some(next)) => current = next,
                    Ok(None) => return,
                    Err(err) => {
                        self.fatal(err);
                        return;
                    }
                }
                continue;
            }
            let next = match self.begin_read(handle) {
                Ok(Some(next)) => next,
                Ok(None) => return,
                Err(err) => {
                    self.fatal(err);
                    return;
                }
            };
            if self.process_completion(None, &current.buf, done.bytes) {
                return;
            }
            current = next;
        }
    }

    // windows.go:301. Reports whether the watch should stop.
    fn process_completion(&self, call_err: Option<u32>, buf: &[u32], bytes: usize) -> bool {
        match call_err {
            Some(ERROR_OPERATION_ABORTED) => return true,
            Some(ERROR_INVALID_PARAMETER) => {
                self.mu.lock().unwrap().buf_bytes = networkBufSize;
                return false;
            }
            Some(ERROR_NOTIFY_ENUM_DIR) => {
                self.dir_watch.events.set_error(ErrOverflow.into());
                self.dir_watch.notify();
                return false;
            }
            Some(ERROR_ACCESS_DENIED) => {
                // Possibly the watched dir was deleted; check and handle.
                let is_dir = std::fs::symlink_metadata(&self.dir_watch.physical_dir)
                    .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0);
                if !is_dir {
                    self.dir_watch.events.remove(&self.dir_watch.dir);
                    self.dir_watch.events.set_error(err_watched_directory_removed());
                    self.dir_watch.notify();
                    self.stop();
                    return true;
                }
                self.fatal(Error::new(format!("unknown error: {}", win32_error(ERROR_ACCESS_DENIED))));
                return true;
            }
            Some(code) => {
                self.fatal(Error::new(format!("unknown error: {}", win32_error(code))));
                return true;
            }
            None => {}
        }

        if bytes == 0 {
            self.dir_watch.events.set_error(ErrOverflow.into());
            self.dir_watch.notify();
            return false;
        }
        // Walk the FILE_NOTIFY_INFORMATION chain: NextEntryOffset, Action and FileNameLength (u32 each), then
        // FileNameLength bytes of UTF-16.
        let data = as_bytes(buf);
        let bytes = bytes.min(data.len());
        let word = |at: usize| data.get(at..at + 4).map_or(0, |w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
        let mut offset = 0usize;
        while offset + 12 <= bytes {
            let next_entry_offset = word(offset);
            let action = word(offset + 4);
            let name_len = word(offset + 8) as usize;
            let Some(name_bytes) = data.get(offset + 12..offset + 12 + name_len) else {
                break;
            };
            let units: Vec<u16> =
                name_bytes.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
            self.process_one(action, &String::from_utf16_lossy(&units));
            if next_entry_offset == 0 {
                break;
            }
            offset += next_entry_offset as usize;
        }
        self.dir_watch.notify();
        false
    }

    // windows.go:355
    fn process_one(&self, action: u32, name: &str) {
        let w = &self.dir_watch;
        let path = format!("{}\\{name}", w.dir);
        let watch_path = format!("{}\\{name}", w.physical_dir);
        match action {
            FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME => {
                // Always emit the event, even if the file is already gone by the time we look it up. The kernel told
                // us it was added, and a subsequent REMOVED needs to find this entry in the eventList so the
                // create+delete pair coalesces away.
                w.events.create(&path);
                if let Ok(meta) = std::fs::symlink_metadata(&watch_path) {
                    self.settle(&path, &meta);
                }
            }
            FILE_ACTION_MODIFIED => {
                if let Ok(meta) = std::fs::symlink_metadata(&watch_path) {
                    if meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY == 0 {
                        w.events.update(&path);
                        self.settle(&path, &meta);
                    }
                }
            }
            FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME => {
                if write_settling() {
                    w.write_finished(&path);
                }
                let seq = w.events.remove_and_get_sequence(&path);
                if w.terminate_callbacks_for_deleted_root(&path, seq, &err_watched_directory_removed()) {
                    w.notify();
                }
            }
            _ => {}
        }
    }

    // tsrs-only (`set_write_settling`): there is no close event, but a save that truncates the file and writes it later
    // is seen empty by the time its event is read. An empty file holds the callbacks like an unclosed one on inotify,
    // until an event shows it non-empty or removed, or the hold times out. A save that is not empty when its event
    // arrives is not delayed.
    fn settle(&self, path: &str, meta: &std::fs::Metadata) {
        if !write_settling() || !meta.is_file() {
            return;
        }
        if meta.len() == 0 {
            self.dir_watch.write_started(path);
        } else {
            self.dir_watch.write_finished(path);
        }
    }

    // windows.go:390. handleWatcherError ends in `close_watch`, which waits for this thread: it runs on another one.
    fn fatal(&self, err: Error) {
        if let Some(backend) = self.backend.upgrade() {
            let werr = dirWatchError { err, dir_watch: Arc::clone(&self.dir_watch) };
            let impl_: Arc<dyn watcherImpl> = backend;
            let spawned = std::thread::Builder::new()
                .name("fswatch-windows-error".to_string())
                .spawn(move || handle_watcher_error(&impl_, &werr));
            if let Err(err) = spawned {
                self.dir_watch.notify_error(&Error::new(format!("unable to report a watch error: {err}")));
            }
        }
        self.stop();
    }

    // windows.go:400. Wakes the thread, which cancels its read and exits; it owns and closes the directory handle.
    fn stop(&self) {
        let mut st = self.mu.lock().unwrap();
        if st.stopped {
            return;
        }
        st.stopped = true;
        self.stop_event.set();
    }
}

fn as_bytes(words: &[u32]) -> &[u8] {
    // SAFETY: any initialized u32 is four initialized bytes, and u8 has no alignment requirement.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), std::mem::size_of_val(words)) }
}

impl watcherImpl for windowsBackend {
    fn base(&self) -> &watcherBase {
        &self.base
    }

    // windows.go:131. Each watch owns its thread, so there is no shared event loop to start.
    fn start(self: Arc<Self>) -> Result<(), Error> {
        self.base.notify_started();
        Ok(())
    }

    // windows.go:424
    fn subscribe(&self, w: &Arc<dirWatch>) -> Result<(), Error> {
        let sub = Arc::new(windowsSubscription::new(Weak::clone(&self.me), w)?);
        let (ready_tx, ready_rx) = mpsc::channel();
        let runner = Arc::clone(&sub);
        let thread = std::thread::Builder::new()
            .name("fswatch-windows".to_string())
            .spawn(move || runner.run(&ready_tx))
            .map_err(|err| Error::new(format!("unable to start the watch thread: {err}")))?;
        let ready = ready_rx
            .recv()
            .unwrap_or_else(|_| Err(Error::new("fswatch: windows: the watch thread ended before it was ready")));
        if let Err(err) = ready {
            let _ = thread.join();
            return Err(err);
        }
        *sub.thread.lock().unwrap() = Some(thread);
        self.subscriptions.lock().unwrap().insert(dwKey(Arc::clone(w)), sub);
        Ok(())
    }

    // windows.go:445. Waits for the watch's thread, so that the directory handle is closed before this returns:
    // re-watching or deleting the directory right away sees a clean slate.
    fn close_watch(&self, w: &Arc<dirWatch>) -> Result<(), Error> {
        let sub = self.subscriptions.lock().unwrap().remove(&dwKey(Arc::clone(w)));
        let Some(sub) = sub else {
            return Ok(());
        };
        sub.stop();
        let thread = sub.thread.lock().unwrap().take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
        Ok(())
    }
}
