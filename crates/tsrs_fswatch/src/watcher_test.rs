// watcher_test.go (the tests that run on macOS / Linux with the ported backends). Each per-backend test runs
// against every available watcher (FSEvents + kqueue on macOS, inotify on Linux).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::debounce::{debounce, defaultMaxWaitTime};
use crate::event::{Event, EventKind};
use crate::pathcompare::pathComparer;
use crate::pathkey::PathComparer;
use crate::testutil_test::*;
use crate::watcher::{
    ErrOverflow, ErrUnavailable, ErrWatchTerminated, Error, WatchCallback, WatchDirectoryRequest,
    WatchOption, all_watchers, default, dirWatch, errNetworkPath, is_in_directory_or_self,
    join_path_suffix, physical_dir_for, rebase_path,
};

const maxWaitTime: Duration = defaultMaxWaitTime;

fn write(path: &str, data: &str) -> TestResult {
    std::fs::write(path, data).map_err(|e| e.to_string())
}

fn mkdir(path: &str) -> TestResult {
    std::fs::create_dir(path).map_err(|e| e.to_string())
}

// watcher_test.go:121
// makeDirSymlink links `link` to the directory `target`: a junction on Windows (no privilege needed), else a symlink.
fn make_dir_symlink(target: &str, link: &str) {
    #[cfg(windows)]
    {
        let (target, link) = (target.replace('/', "\\"), link.replace('/', "\\"));
        let junction = std::process::Command::new("cmd").args(["/c", "mklink", "/J", &link, &target]).output();
        if junction.is_ok_and(|out| out.status.success()) {
            return;
        }
        std::os::windows::fs::symlink_dir(&target, &link).unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

// watcher_test.go:578
#[test]
fn path_comparer() {
    let tests: &[(&str, &str, &str, bool, bool)] = &[
        ("/root", "/root", "", true, true),
        ("/root", "/root/file.ts", "/file.ts", true, true),
        ("/root", "/ROOT", "", false, true),
        ("/root", "/ROOT/File.ts", "/File.ts", false, true),
        ("/root", "/ROOT/Nested/File.ts", "/Nested/File.ts", false, true),
        ("/root", "/ROOT2/File.ts", "", false, false),
        ("/root", "/roo", "", false, false),
        ("/root/sub", "/ROOT", "", false, false),
        ("/root", "/other/File.ts", "", false, false),
        ("/root", "/ROOTish/File.ts", "", false, false),
        ("/root/sub", "/ROOT/SUB", "", false, true),
        ("/root/sub", "/ROOT/su", "", false, false),
        ("/root/[", "/ROOT/{/File.ts", "", false, false),
        ("/root/@", "/ROOT/`/File.ts", "", false, false),
        ("/", "/File.ts", "File.ts", true, true),
        ("/", "/", "", true, true),
        ("", "", "", false, false),
        ("", "/File.ts", "", false, false),
        ("/caf\u{00e9}", "/CAF\u{00c9}/File.ts", "/File.ts", false, true),
        ("/s", "/\u{017f}/File.ts", "/File.ts", false, true),
        ("/\u{017f}", "/S/File.ts", "/File.ts", false, true),
        ("/s/sub", "/\u{017f}/SUB/File.ts", "/File.ts", false, true),
        ("/\u{017f}/sub", "/S/SUB/File.ts", "/File.ts", false, true),
        ("/s", "/\u{017f}oo/File.ts", "", false, false),
        ("/k", "/\u{212a}/File.ts", "/File.ts", false, true),
        ("/\u{03c3}", "/\u{03c2}/File.ts", "/File.ts", false, true),
        ("/\u{00e9}", "/\u{00c8}/File.ts", "", false, false),
        ("/\u{00df}", "/SS/File.ts", "/File.ts", false, crate::canonicalize::nativePathFolding),
        ("/root/s", "/ROOT/\u{017f}/File.ts", "/File.ts", false, true),
        ("/root/\u{017f}", "/ROOT/S", "", false, true),
    ];
    let mut failures = Vec::new();
    for &(root, path, suffix, exact, ignore_case_want) in tests {
        for ignore_case in [false, true] {
            let comparer = pathComparer { ignore_case };
            let want = if ignore_case { ignore_case_want } else { exact };
            let got = comparer.suffix(root, path);
            if got.is_some() != want || got.as_deref().is_some_and(|s| s != suffix) {
                failures.push(format!("suffix({root:?}, {path:?}), ignoreCase={ignore_case}: got {got:?}, want ({suffix:?}, {want})"));
            }
            if comparer.contains(root, path) != want {
                failures.push(format!(
                    "contains({root:?}, {path:?}), ignoreCase={ignore_case}: want {want}"
                ));
            }
            failures.extend((["/display", "/"]).into_iter().filter_map(|to| {
                let rebased = comparer.rebase(path, root, to);
                if rebased.is_some() != want || rebased.as_ref().is_some_and(|r| *r != join_path_suffix(to, suffix)) {
                    Some(format!("rebase({path:?}, {root:?}, {to:?}), ignoreCase={ignore_case}: got {rebased:?}"))
                } else {
                    None
                }
            }));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// watcher_test.go:664
#[test]
fn file_callback_case_sensitivity() {
    for ignore_case in [false, true] {
        let got: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let dw = dirWatch::new(
            "/root".to_string(),
            "/root".to_string(),
            &debounce::new(),
            pathComparer { ignore_case },
            None,
            true,
        );
        let sink = std::sync::Arc::clone(&got);
        let cb: WatchCallback = Arc::new(move |events: Vec<Event>, _: Option<Error>| {
            sink.lock().unwrap().extend(events);
        });
        dw.add_callback("/root", "/root", false, cb, None, "/root/file.ts");
        dw.events.update("/root/FILE.ts");
        dw.events.update("/root/other.ts");
        dw.trigger_callbacks_for_test();
        let got = got.lock().unwrap();
        if ignore_case {
            assert!(
                got.len() == 1 && got[0].path == "/root/file.ts",
                "case-insensitive callback: got {got:?}"
            );
        } else {
            assert!(got.is_empty(), "case-sensitive callback: got {got:?}");
        }
        dw.destroy_debounce_for_test();
    }
}

// watcher_test.go:686
#[test]
fn path_comparer_exact_keys() {
    let c = PathComparer::default();
    for path in ["/A/File.ts", "/straße/İ.ts", "/cafe\u{0301}.ts"] {
        assert_eq!(c.key(path), path);
    }
    assert!(
        c.rebase("/A/file.ts", "/a", "/target").is_none(),
        "zero comparer must use exact matching"
    );
    assert_eq!(c.rebase("/a/file.ts", "/a", "/target").as_deref(), Some("/target/file.ts"));
}

// watcher_test.go:702
#[test]
fn rebase_path_cases() {
    let cases = [
        ("/from", "/from", "/to", "/to"),
        ("/from/child", "/from", "/to", "/to/child"),
        ("/from-sibling/child", "/from", "/to", "/from-sibling/child"),
        ("/child", "/", "/to", "/to/child"),
        ("/from/child", "/from", "/", "/child"),
    ];
    for (path, from, to, want) in cases {
        let (path, from, to, want) = (native(path), native(from), native(to), native(want));
        assert_eq!(rebase_path(&path, &from, &to), want, "rebasePath({path:?}, {from:?}, {to:?})");
    }
}

// Go's helpers on Windows: either separator matches, and a joined path uses `\`.
#[cfg(windows)]
#[test]
fn rebase_path_cases_windows() {
    let cases = [
        (r"C:\from", r"C:\from", r"D:\to", r"D:\to"),
        (r"C:\from\child", r"C:\from", r"D:\to", r"D:\to\child"),
        ("C:/from/child", "C:/from", r"D:\to", r"D:\to/child"),
        (r"C:\from-sibling\child", r"C:\from", r"D:\to", r"C:\from-sibling\child"),
        (r"C:\from\child", r"C:\from\", r"D:\to", r"D:\to\child"),
        (r"C:\child", r"C:\", r"D:\to", r"D:\to\child"),
        (r"C:\from\child", r"C:\from", r"D:\", r"D:\child"),
    ];
    for (path, from, to, want) in cases {
        assert_eq!(rebase_path(path, from, to), want, "rebasePath({path:?}, {from:?}, {to:?})");
    }
}

#[cfg(windows)]
#[test]
fn join_path_suffix_windows() {
    let cases = [
        (r"D:\to", "", r"D:\to"),
        (r"D:\to", "child", r"D:\to\child"),
        (r"D:\to\", "child", r"D:\to\child"),
        (r"D:\to", r"\child", r"D:\to\child"),
        (r"D:\to\", r"\child", r"D:\to\child"),
        (r"D:\to", "/child", r"D:\to/child"),
    ];
    for (root, suffix, want) in cases {
        assert_eq!(join_path_suffix(root, suffix), want, "joinPathSuffix({root:?}, {suffix:?})");
    }
}

#[cfg(windows)]
#[test]
fn is_in_directory_or_self_and_direct_child_windows() {
    use crate::watcher::is_direct_child;

    let in_dir = [
        (r"C:\parent", r"C:\parent", true),
        (r"C:\parent", r"C:\parent\child", true),
        (r"C:\parent", "C:\\parent/child", true),
        (r"C:\parent", r"C:\parent\child\nested", true),
        (r"C:\parent", r"C:\parent-sibling", false),
        (r"C:\parent", r"C:\other\child", false),
        (r"C:\", r"C:\child", true),
        ("", r"C:\child", false),
    ];
    for (dir, path, want) in in_dir {
        assert_eq!(is_in_directory_or_self(dir, path), want, "isInDirectoryOrSelf({dir:?}, {path:?})");
    }
    let direct = [
        (r"C:\parent", r"C:\parent\child", true),
        (r"C:\parent", "C:\\parent/child", true),
        (r"C:\parent", r"C:\parent\child\nested", false),
        (r"C:\parent", r"C:\parent", false),
        (r"C:\parent", r"C:\parent-sibling", false),
        (r"C:\parent", r"C:\parent\", false),
    ];
    for (dir, path, want) in direct {
        assert_eq!(is_direct_child(dir, path), want, "isDirectChild({dir:?}, {path:?})");
    }
}

#[test]
fn physical_dir_for_has_no_verbatim_prefix() {
    let mut t = TestT::new(1);
    let dir = t.temp_dir();
    let physical = physical_dir_for(&dir);
    assert!(!physical.starts_with(r"\\?\"), "physical_dir_for({dir:?}) = {physical:?}");
    assert!(crate::filepath::HOST.is_abs(&physical), "physical_dir_for({dir:?}) = {physical:?}");
}

// watcher_test.go:764
#[test]
fn physical_dir_for_resolves_symlink_ancestor() {
    let mut t = TestT::new(1);
    let root = t.temp_dir();
    let target = format!("{root}/target");
    std::fs::create_dir_all(format!("{target}/nested")).unwrap();
    let link = format!("{root}/link");
    make_dir_symlink(&target, &link);

    let dir = format!("{link}/nested");
    let want = physical_dir_for(&format!("{target}/nested"));
    assert_eq!(physical_dir_for(&dir), want);
}

// watcher_test.go:782
#[test]
fn is_in_directory_or_self_cases() {
    let cases = [
        ("/parent", "/parent", true),
        ("/parent", "/parent/child", true),
        ("/parent", "/parent/child/nested", true),
        ("/parent", "/parent-sibling", false),
        ("/", "/", true),
        ("/", "/child", true),
        ("", "/parent/child", false),
    ];
    for (dir, path, want) in cases {
        assert_eq!(
            is_in_directory_or_self(dir, path),
            want,
            "isInDirectoryOrSelf({dir:?}, {path:?})"
        );
    }
}

// ----- files -------------------------------------------------------------

// watcher_test.go:853
#[test]
fn watch_file_create() {
    run_for_each_watcher("TestWatchFileCreate", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f = sub_path(&dir);
        write(&f, "hello")?;
        expect_event_sequence(&r, &[want(EventKind::Update, &f)])?;
        Ok(())
    });
}

// watcher_test.go:867
#[test]
fn watch_file_update() {
    run_for_each_watcher("TestWatchFileUpdate", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f = sub_path(&dir);
        write(&f, "v1")?;
        let _ = r.wait_for_event(r.deadline(), |_| true); // consume the create event
        write(&f, "v2-longer")?;
        expect_event_sequence(&r, &[want(EventKind::Update, &f)])?;
        Ok(())
    });
}

// watcher_test.go:887
#[test]
fn watch_file_rename() {
    run_for_each_watcher("TestWatchFileRename", |t, w| {
        let dir = new_tmp_dir(t);
        let f1 = sub_path(&dir);
        let f2 = sub_path(&dir);
        write(&f1, "x")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        std::fs::rename(&f1, &f2).map_err(|e| e.to_string())?;
        expect_event_set(&r, vec![want(EventKind::Delete, &f1), want(EventKind::Update, &f2)])?;
        Ok(())
    });
}

// watcher_test.go:928
#[test]
fn watch_file_delete() {
    run_for_each_watcher("TestWatchFileDelete", |t, w| {
        let dir = new_tmp_dir(t);
        let f = sub_path(&dir);
        write(&f, "x")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        std::fs::remove_file(&f).map_err(|e| e.to_string())?;
        expect_event_sequence(&r, &[want(EventKind::Delete, &f)])?;
        Ok(())
    });
}

// ----- directories -------------------------------------------------------

// watcher_test.go:946
#[test]
fn subscribe_dir_create() {
    run_for_each_watcher("TestSubscribeDirCreate", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f = sub_path(&dir);
        mkdir(&f)?;
        expect_event_sequence(&r, &[want(EventKind::Update, &f)])?;
        Ok(())
    });
}

// watcher_test.go:965
#[test]
fn subscribe_non_ascii_path() {
    run_for_each_watcher("TestSubscribeNonASCIIPath", |t, w| {
        let parent = new_tmp_dir(t);
        // "café" + "résumé"; both precomposed NFC.
        let dir = join(&parent, "caf\u{00e9}-dir");
        mkdir(&dir)?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let child = join(&dir, "r\u{00e9}sum\u{00e9}.txt");
        write(&child, "hi")?;
        expect_event_sequence(&r, &[want(EventKind::Update, &child)])?;
        Ok(())
    });
}

// watcher_test.go:1004
#[test]
fn subscribe_dir_delete() {
    run_for_each_watcher("TestSubscribeDirDelete", |t, w| {
        let dir = new_tmp_dir(t);
        let f = sub_path(&dir);
        mkdir(&f)?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        std::fs::remove_dir_all(&f).map_err(|e| e.to_string())?;
        expect_event_sequence(&r, &[want(EventKind::Delete, &f)])?;
        Ok(())
    });
}

// watcher_test.go:1020
#[test]
fn subscribe_watched_dir_deleted() {
    run_for_each_watcher("TestSubscribeWatchedDirDeleted", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
        expect_event_sequence(&r, &[want(EventKind::Delete, &dir)])?;

        // Give the backend a moment to surface ErrWatchTerminated alongside
        // the delete; some backends batch the error into a later debounce
        // tick than the event itself.
        let deadline = std::time::Instant::now() + r.deadline();
        while std::time::Instant::now() < deadline && r.err_count() == 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
        let errs = r.take_errs();
        if !errs.iter().any(|e| e.is(ErrWatchTerminated)) {
            fatalf!("expected ErrWatchTerminated after watched dir delete, got errs={:?}", errs);
        }

        // Re-create; should not emit events for a now-stale watch.
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let extra = r.drain_quiet(Duration::from_millis(200));
        if !extra.is_empty() {
            fatalf!("expected no follow-up events, got {:?}", extra);
        }
        Ok(())
    });
}

// ----- sub-files ---------------------------------------------------------

// watcher_test.go:1071
#[test]
fn subscribe_subfile_create() {
    run_for_each_watcher("TestSubscribeSubfileCreate", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        let sub = sub_path(&dir);
        mkdir(&sub)?;
        expect_contains(&r, EventKind::Update, &sub)?;
        std::thread::sleep(Duration::from_millis(100));
        let f = sub_path(&sub);
        write(&f, "hi")?;
        expect_event_sequence(&r, &[want(EventKind::Update, &f)])?;
        Ok(())
    });
}

// watcher_test.go:1142
#[test]
fn subscribe_subfile_delete() {
    run_for_each_watcher("TestSubscribeSubfileDelete", |t, w| {
        let dir = new_tmp_dir(t);
        let sub = sub_path(&dir);
        mkdir(&sub)?;
        let f = sub_path(&sub);
        write(&f, "x")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        std::fs::remove_file(&f).map_err(|e| e.to_string())?;
        let want_ = vec![want(EventKind::Delete, &f)];
        let got = r.wait_for_all(r.deadline(), &want_);
        assert_event_sequence(&filter_events_for_paths(&got, &[&f]), &want_)?;
        Ok(())
    });
}

// watcher_test.go:1187
#[test]
fn subscribe_subdir_delete_with_files() {
    run_for_each_watcher("TestSubscribeSubdirDeleteWithFiles", |t, w| {
        let dir = new_tmp_dir(t);
        let sub_dir = sub_path(&dir);
        mkdir(&sub_dir)?;
        let child = sub_path(&sub_dir);
        write(&child, "x")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        std::fs::remove_dir_all(&sub_dir).map_err(|e| e.to_string())?;
        expect_event_set(
            &r,
            vec![want(EventKind::Delete, &sub_dir), want(EventKind::Delete, &child)],
        )?;
        Ok(())
    });
}

// watcher_test.go:1438
#[test]
fn subscribe_multiple_same_dir() {
    run_for_each_watcher("TestSubscribeMultipleSameDir", |t, w| {
        let dir = new_tmp_dir(t);
        std::thread::sleep(Duration::from_millis(50));
        let r1 = new_recorder(t, Some(w));
        let s1 = w.watch_directory(&dir, r1.callback(), vec![]).map_err(|e| e.to_string())?;
        let r2 = new_recorder(t, Some(w));
        let s2 = w.watch_directory(&dir, r2.callback(), vec![]).map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(100));
        let f = sub_path(&dir);
        write(&f, "hi")?;
        let res = assert_event_sequence(&r1.next(r1.deadline()), &[want(EventKind::Update, &f)])
            .and_then(|()| {
                assert_event_sequence(&r2.next(r2.deadline()), &[want(EventKind::Update, &f)])
            });
        let _ = s1.close();
        let _ = s2.close();
        res
    });
}

// watcher_test.go:1810
#[test]
fn subscribe_missing_dir_error() {
    run_for_each_watcher("TestSubscribeMissingDirError", |t, w| {
        let bogus = format!("{}/definitely-not-here", new_tmp_dir(t));
        if w.watch_directory(&bogus, noop_callback(), vec![]).is_ok() {
            fatalf!("expected error subscribing to non-existent dir");
        }
        Ok(())
    });
}

// watcher_test.go:1821
#[test]
fn subscribe_not_a_dir_error() {
    run_for_each_watcher("TestSubscribeNotADirError", |t, w| {
        let dir = new_tmp_dir(t);
        let f = sub_path(&dir);
        write(&f, "x")?;
        if w.watch_directory(&f, noop_callback(), vec![]).is_ok() {
            fatalf!("expected error subscribing to a file");
        }
        Ok(())
    });
}

// watcher_test.go:1845
#[test]
fn subscribe_rejects_relative_path() {
    run_for_each_watcher("TestSubscribeRejectsRelativePath", |_, w| {
        if w.watch_directory("relative/path", noop_callback(), vec![]).is_ok() {
            fatalf!("WatchDirectory with relative path should return an error");
        }
        if w.watch_file("relative/path/file.txt", noop_callback()).is_ok() {
            fatalf!("WatchFile with relative path should return an error");
        }
        Ok(())
    });
}

#[test]
fn watch_rejects_network_paths() {
    run_for_each_watcher("TestWatchRejectsNetworkPaths", |t, w| {
        let dir = new_tmp_dir(t);
        let mut network_paths =
            vec![r"\\server\share".to_string(), r"\??\UNC\server\share".to_string()];
        if !cfg!(windows) {
            // This spells an existing local directory on POSIX; reject it
            // before filepath cleaning can erase its network prefix.
            network_paths.push(format!("/{dir}"));
        }
        for network_path in network_paths {
            let err = w
                .watch_directories(vec![
                    WatchDirectoryRequest {
                        dir: dir.clone(),
                        callback: Some(noop_callback()),
                        options: vec![],
                    },
                    WatchDirectoryRequest {
                        dir: network_path.clone(),
                        callback: Some(noop_callback()),
                        options: vec![],
                    },
                ])
                .err()
                .expect("network watch must fail");
            assert!(err.is(errNetworkPath), "WatchDirectories({network_path:?}): {err}");
            let file = format!("{network_path}/file.ts");
            let err = w.watch_file(&file, noop_callback()).err().expect("network watch must fail");
            assert!(err.is(errNetworkPath), "WatchFile({file:?}): {err}");
        }
        Ok(())
    });
}

// watcher_test.go:1861
#[test]
fn subscribe_unsubscribe_idempotent() {
    run_for_each_watcher("TestSubscribeUnsubscribeIdempotent", |t, w| {
        let dir = new_tmp_dir(t);
        let r = new_recorder(t, Some(w));
        let sub = w.watch_directory(&dir, r.callback(), vec![]).map_err(|e| e.to_string())?;
        sub.close().map_err(|e| e.to_string())?;
        if let Err(e) = sub.close() {
            fatalf!("second Close should be a no-op, got {}", e);
        }
        Ok(())
    });
}

// watcher_test.go:1888
#[test]
fn subscribe_close_then_re_subscribe() {
    run_for_each_watcher("TestSubscribeCloseThenReSubscribe", |t, w| {
        let dir = new_tmp_dir(t);
        let r1 = new_recorder(t, Some(w));
        let s1 = w.watch_directory(&dir, r1.callback(), vec![]).map_err(|e| e.to_string())?;
        s1.close().map_err(|e| e.to_string())?;

        let r2 = new_recorder(t, Some(w));
        let s2 = w
            .watch_directory(&dir, r2.callback(), vec![])
            .map_err(|e| format!("re-WatchDirectory after Close: {e}"))?;
        std::thread::sleep(settle_sleep(w));

        let f = sub_path(&dir);
        write(&f, "hi")?;
        let res = expect_event_sequence(&r2, &[want(EventKind::Update, &f)]);
        let _ = s2.close();
        res?;

        let stale = r1.drain_quiet(Duration::from_millis(50));
        if !stale.is_empty() {
            fatalf!("closed watch saw events: {:?}", to_want_events(&stale));
        }
        Ok(())
    });
}

// watcher_test.go:2163
#[test]
fn subscribe_no_events_after_unsubscribe() {
    run_for_each_watcher("TestSubscribeNoEventsAfterUnsubscribe", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, sub) = subscribe_for(t, &dir, w)?;
        sub.close().map_err(|e| e.to_string())?;
        let f = sub_path(&dir);
        write(&f, "x")?;
        let got = r.drain_quiet(Duration::from_millis(500));
        if !got.is_empty() {
            fatalf!("expected no events after closeWatch, got {:?}", to_want_events(&got));
        }
        Ok(())
    });
}

// watcher_test.go:2344
#[test]
fn default_backend_matches_platform() {
    let d = default();
    let want_name = if cfg!(target_os = "linux") {
        "inotify"
    } else if cfg!(target_os = "macos") {
        "fsevents"
    } else if cfg!(windows) {
        "windows"
    } else {
        return;
    };
    assert!(d.available());
    assert_eq!(d.name(), want_name);
}

// watcher_test.go:2374
#[test]
fn unavailable_backend_returns_error() {
    let Some(unavailable) = all_watchers().into_iter().find(|w| !w.available()) else {
        return;
    };
    let mut t = TestT::new(1);
    let dir = new_tmp_dir(&mut t);
    let err = unavailable
        .watch_directory(&dir, noop_callback(), vec![])
        .err()
        .expect("expected an error");
    assert!(
        err.is(ErrUnavailable),
        "expected ErrUnavailable from {}, got {}",
        unavailable.name(),
        err
    );
}

// watcher_test.go:2420
#[test]
fn non_recursive_file_create() {
    run_for_each_watcher("TestNonRecursiveFileCreate", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for_opts(t, &dir, w, vec![])?;
        let f = sub_path(&dir);
        write(&f, "hello")?;
        expect_event_sequence(&r, &[want(EventKind::Update, &f)])?;
        Ok(())
    });
}

// watcher_test.go:2483
#[test]
fn non_recursive_grandchild_ignored() {
    run_for_each_watcher("TestNonRecursiveGrandchildIgnored", |t, w| {
        let dir = new_tmp_dir(t);
        let sub = join(&dir, "child");
        mkdir(&sub)?;
        let (r, _) = subscribe_for_opts(t, &dir, w, vec![])?;
        let grandchild = sub_path(&sub);
        write(&grandchild, "deep")?;
        let marker = sub_path(&dir);
        write(&marker, "flush")?;
        let mut got = expect_contains(&r, EventKind::Update, &marker)?;
        got.extend(r.drain_quiet(2 * maxWaitTime));
        assert_no_events_for_path(&got, &grandchild, "expected no events for grandchild")
    });
}

// watcher_test.go:2608
#[test]
fn file_watch_create() {
    run_for_each_watcher("TestFileWatchCreate", |t, w| {
        let dir = new_tmp_dir(t);
        let f = join(&dir, "target.txt");
        let (r, _) = subscribe_file_for(t, &f, w)?;
        write(&f, "hello")?;
        expect_event_sequence(&r, &[want(EventKind::Update, &f)])?;
        Ok(())
    });
}

// watcher_test.go:2641
#[test]
fn file_watch_delete() {
    run_for_each_watcher("TestFileWatchDelete", |t, w| {
        let dir = new_tmp_dir(t);
        let f = join(&dir, "target.txt");
        write(&f, "x")?;
        let (r, _) = subscribe_file_for(t, &f, w)?;
        std::fs::remove_file(&f).map_err(|e| e.to_string())?;
        expect_event_sequence(&r, &[want(EventKind::Delete, &f)])?;
        Ok(())
    });
}

// watcher_test.go:2659
#[test]
fn file_watch_ignores_siblings() {
    run_for_each_watcher("TestFileWatchIgnoresSiblings", |t, w| {
        let dir = new_tmp_dir(t);
        let target = join(&dir, "target.txt");
        let sibling = join(&dir, "sibling.txt");
        let (r, _) = subscribe_file_for(t, &target, w)?;
        let (witness, _) = subscribe_for_opts(t, &dir, w, vec![])?;
        write(&sibling, "noise")?;
        expect_contains(&witness, EventKind::Update, &sibling)?;
        expect_no_buffered_events(&r, "expected no events for sibling")
    });
}

// watcher_test.go:2809
#[test]
fn atomic_save() {
    run_for_each_watcher("TestAtomicSave", |t, w| {
        let dir = new_tmp_dir(t);
        let target = join(&dir, "config.json");
        write(&target, r#"{"v":1}"#)?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let tmp = format!("{target}.tmp");
        write(&tmp, r#"{"v":2}"#)?;
        std::fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
        let got = r.wait_for_event(r.deadline(), |e| e.path == target);
        if filter_events_for_paths(&got, &[&target]).is_empty() {
            fatalf!("expected events for {} after atomic save, got none", target);
        }
        Ok(())
    });
}

// watcher_test.go:984
#[test]
fn subscribe_dir_rename() {
    run_for_each_watcher("TestSubscribeDirRename", |t, w| {
        let dir = new_tmp_dir(t);
        let f1 = sub_path(&dir);
        mkdir(&f1)?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f2 = sub_path(&dir);
        std::fs::rename(&f1, &f2).map_err(|e| e.to_string())?;
        expect_event_set(&r, vec![want(EventKind::Delete, &f1), want(EventKind::Update, &f2)])?;
        Ok(())
    });
}

// watcher_test.go:1094
#[test]
fn subscribe_subfile_update() {
    run_for_each_watcher("TestSubscribeSubfileUpdate", |t, w| {
        let dir = new_tmp_dir(t);
        let sub = sub_path(&dir);
        mkdir(&sub)?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f = sub_path(&sub);
        // WatchDirectory-then-create so the create event populates the watcherImpl's tree before the modify arrives.
        write(&f, "v1")?;
        let _ = r.wait_for_event(r.deadline(), |_| true);
        write(&f, "v2-longer")?;
        expect_contains(&r, EventKind::Update, &f)?;
        Ok(())
    });
}

// watcher_test.go:1117
#[test]
fn subscribe_subfile_rename() {
    run_for_each_watcher("TestSubscribeSubfileRename", |t, w| {
        let dir = new_tmp_dir(t);
        let sub = sub_path(&dir);
        mkdir(&sub)?;
        let f1 = sub_path(&sub);
        write(&f1, "x")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f2 = sub_path(&sub);
        std::fs::rename(&f1, &f2).map_err(|e| e.to_string())?;
        // Wait for both events to arrive before checking the set.
        let want = vec![want(EventKind::Delete, &f1), want(EventKind::Update, &f2)];
        let got = r.wait_for_all(r.deadline(), &want);
        assert_event_set(&filter_events_for_paths(&got, &[&f1, &f2]), want)
    });
}

// watcher_test.go:1167
#[test]
fn subscribe_subdir_create() {
    run_for_each_watcher("TestSubscribeSubdirCreate", |t, w| {
        let dir = new_tmp_dir(t);
        let sub = sub_path(&dir);
        mkdir(&sub)?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let nested = sub_path(&sub);
        mkdir(&nested)?;
        let want = [want(EventKind::Update, &nested)];
        let got = r.wait_for_all(r.deadline(), &want);
        assert_event_sequence(&filter_events_for_paths(&got, &[&nested]), &want)
    });
}

// watcher_test.go:1314
#[test]
fn subscribe_coalesce_create_update() {
    run_for_each_watcher("TestSubscribeCoalesceCreateUpdate", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f = sub_path(&dir);
        write(&f, "v1")?;
        write(&f, "v2")?;
        // The two writes should net to one update. Under host load the debounce may split them across batches, so
        // check the coalesced effect via replay_event_list rather than insisting on a single delivered event.
        let got = r.gather_until_quiet(r.deadline(), 3 * maxWaitTime);
        let net = replay_event_list(&filter_events_for_paths(&got, &[&f]));
        assert_event_set(&net, vec![want(EventKind::Update, &f)])
    });
}

// watcher_test.go:1336
#[test]
fn subscribe_coalesce_delete_create_as_update() {
    run_for_each_watcher("TestSubscribeCoalesceDeleteCreateAsUpdate", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f = sub_path(&dir);
        write(&f, "v1")?;
        let _ = r.wait_for_event(r.deadline(), |_| true);
        std::fs::remove_file(&f).map_err(|e| e.to_string())?;
        write(&f, "v2")?;
        // Net: delete+create coalesces to update.
        let got = r.gather_until_quiet(r.deadline(), 3 * maxWaitTime);
        let net = replay_event_list(&filter_events_for_paths(&got, &[&f]));
        assert_event_set(&net, vec![want(EventKind::Update, &f)])
    });
}

// watcher_test.go:1359
#[test]
fn subscribe_coalesce_create_then_delete() {
    run_for_each_watcher("TestSubscribeCoalesceCreateThenDelete", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f1 = sub_path(&dir);
        let f2 = sub_path(&dir);
        write(&f1, "x")?;
        write(&f2, "x")?;
        std::fs::remove_file(&f2).map_err(|e| e.to_string())?;
        // Whether all three operations land in one debounce batch (perfect coalescing, just [update f1]) or split
        // across batches depends on kernel timing. Either is correct as long as the net effect, replaying the events
        // through eventList, leaves only [update f1]. The quiet window must exceed the debouncer's maxWaitTime so a
        // delayed follow-up batch isn't cut off.
        let got = r.gather_until_quiet(r.deadline(), 3 * maxWaitTime);
        let net = replay_event_list(&got);
        assert_event_set(&net, vec![want(EventKind::Update, &f1)])
    });
}

// watcher_test.go:1389
#[test]
fn subscribe_coalesce_multiple_updates() {
    run_for_each_watcher("TestSubscribeCoalesceMultipleUpdates", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f = sub_path(&dir);
        write(&f, "v1")?;
        let _ = r.wait_for_event(r.deadline(), |_| true); // consume initial update
        for v in ["v2", "v3", "v4"] {
            write(&f, v)?;
        }
        let got = r.gather_until_quiet(r.deadline(), 3 * maxWaitTime);
        let net = replay_event_list(&filter_events_for_paths(&got, &[&f]));
        assert_event_set(&net, vec![want(EventKind::Update, &f)])
    });
}

// watcher_test.go:1410
#[test]
fn subscribe_coalesce_update_delete() {
    run_for_each_watcher("TestSubscribeCoalesceUpdateDelete", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f = sub_path(&dir);
        // Upstream's debouncer (by design) fires the first event in a quiet window immediately. To exercise the
        // coalescing path, create the file post-subscribe and consume that initial event so the debouncer's last
        // time is recent before the update+delete pair.
        write(&f, "v1")?;
        let _ = r.wait_for_event(r.deadline(), |_| true);
        write(&f, "v2")?;
        std::fs::remove_file(&f).map_err(|e| e.to_string())?;
        let got = r.gather_until_quiet(r.deadline(), 3 * maxWaitTime);
        let net = replay_event_list(&filter_events_for_paths(&got, &[&f]));
        assert_event_set(&net, vec![want(EventKind::Delete, &f)])
    });
}

// watcher_test.go:1471
#[test]
fn subscribe_multiple_different_dirs() {
    run_for_each_watcher("TestSubscribeMultipleDifferentDirs", |t, w| {
        let dir1 = new_tmp_dir(t);
        let dir2 = new_tmp_dir(t);
        let (r1, _) = subscribe_for(t, &dir1, w)?;
        let (r2, _) = subscribe_for(t, &dir2, w)?;
        let f1 = sub_path(&dir1);
        let f2 = sub_path(&dir2);
        write(&f1, "a")?;
        write(&f2, "b")?;
        assert_event_sequence(&r1.next(r1.deadline()), &[want(EventKind::Update, &f1)])?;
        assert_event_sequence(&r2.next(r2.deadline()), &[want(EventKind::Update, &f2)])
    });
}

// watcher_test.go:1978
#[test]
fn subscribe_deep_nested_create() {
    run_for_each_watcher("TestSubscribeDeepNestedCreate", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        // Create a/b/c one level at a time so the watcher can keep up.
        let a = join(&dir, "a");
        let b = join(&a, "b");
        let c = join(&b, "c");
        for d in [&a, &b, &c] {
            mkdir(d)?;
            std::thread::sleep(Duration::from_millis(150));
        }
        let f = join(&c, "deep.txt");
        write(&f, "deep")?;
        let want = [want(EventKind::Update, &a), want(EventKind::Update, &f)];
        let got = r.wait_for_all(r.deadline(), &want);
        for w in &want {
            if !got.iter().any(|e| e.kind.0 == w.0 && e.path == w.1) {
                fatalf!("expected {:?} for {}, got {:?}", w.0, w.1, to_want_events(&got));
            }
        }
        Ok(())
    });
}

// watcher_test.go:2008
#[test]
fn subscribe_many_files_at_once() {
    run_for_each_watcher("TestSubscribeManyFilesAtOnce", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for(t, &dir, w)?;
        let paths: Vec<String> = std::iter::repeat_with(|| sub_path(&dir)).take(50).collect();
        for p in &paths {
            write(p, "x")?;
        }
        let want: Vec<wantEvent> = paths.iter().map(|p| want(EventKind::Update, p)).collect();
        // Retry the missing files (a fresh write provokes a new event) a couple of times before failing.
        let mut got = r.wait_for_all(r.deadline(), &want);
        let has = |got: &[Event], p: &str| contains_event(got, EventKind::Update, p);
        for _ in 0..3 {
            if paths.iter().all(|p| has(&got, p)) {
                break;
            }
            for p in paths.iter().filter(|p| !has(&got, p)) {
                let _ = std::fs::write(p, "x");
            }
            got.extend(r.wait_for_all(r.deadline(), &want));
        }
        for p in &paths {
            if !has(&got, p) {
                fatalf!("missing create for {} (got {} events total)", p, got.len());
            }
        }
        Ok(())
    });
}

// watcher_test.go:2049
#[test]
fn subscribe_truncate_file() {
    run_for_each_watcher("TestSubscribeTruncateFile", |t, w| {
        let dir = new_tmp_dir(t);
        let f = sub_path(&dir);
        write(&f, "hello world")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let file = std::fs::OpenOptions::new().write(true).open(&f).map_err(|e| e.to_string())?;
        file.set_len(0).map_err(|e| e.to_string())?;
        drop(file);
        expect_event_sequence(&r, &[want(EventKind::Update, &f)])?;
        Ok(())
    });
}

// watcher_test.go:2088
#[test]
fn subscribe_rename_dir() {
    run_for_each_watcher("TestSubscribeRenameDir", |t, w| {
        let dir = new_tmp_dir(t);
        let sub = join(&dir, "before");
        mkdir(&sub)?;
        write(&join(&sub, "file.txt"), "x")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let after = join(&dir, "after");
        std::fs::rename(&sub, &after).map_err(|e| e.to_string())?;
        let want = [want(EventKind::Update, &after), want(EventKind::Delete, &sub)];
        let got = r.wait_for_all(r.deadline(), &want);
        for w in &want {
            if !got.iter().any(|e| e.kind.0 == w.0 && e.path == w.1) {
                fatalf!("expected {:?} for {}, got {:?}", w.0, w.1, to_want_events(&got));
            }
        }
        Ok(())
    });
}

// watcher_test.go:2116
#[test]
fn subscribe_replace_file_with_dir() {
    run_for_each_watcher("TestSubscribeReplaceFileWithDir", |t, w| {
        let dir = new_tmp_dir(t);
        let target = sub_path(&dir);
        write(&target, "file")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        std::fs::remove_file(&target).map_err(|e| e.to_string())?;
        mkdir(&target)?;
        // Should see at least one event for target (delete and/or update).
        let got = r.wait_for_event(r.deadline(), |e| e.path == target);
        if !contains_event(&got, EventKind::Delete, &target)
            && !contains_event(&got, EventKind::Update, &target)
        {
            fatalf!("expected events for file-to-dir replacement, got {:?}", to_want_events(&got));
        }
        Ok(())
    });
}

// watcher_test.go:2142
#[test]
fn subscribe_append_to_file() {
    run_for_each_watcher("TestSubscribeAppendToFile", |t, w| {
        let dir = new_tmp_dir(t);
        let f = sub_path(&dir);
        write(&f, "initial")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let mut fh = std::fs::OpenOptions::new().append(true).open(&f).map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut fh, b" appended").map_err(|e| e.to_string())?;
        drop(fh);
        expect_event_sequence(&r, &[want(EventKind::Update, &f)])?;
        Ok(())
    });
}

// The retry loop that TestRecursiveMoveInPrePopulated, TestRecreateSubdirAndModify and TestReplaceParentDirWithDifferent
// each spell out: the watch may not yet be armed on a moved-in or recreated subtree, so write a fresh file until an
// update for a file under `dir` surfaces, rather than betting on the first write being seen.
fn nudge_until_update_under(
    r: &recordingWatcher,
    dir: &str,
    file_prefix: &str,
    total: Duration,
    what: &str,
) -> TestResult {
    let prefix = format!("{dir}{}", char::from(crate::filepath::HOST.separator()));
    let is_update_under = |e: &Event| e.kind == EventKind::Update && e.path.starts_with(&prefix);
    let deadline = Instant::now() + total;
    let mut all_seen = Vec::new();
    let mut attempt = 0;
    while Instant::now() < deadline {
        write(&join(dir, &format!("{file_prefix}-{attempt}.txt")), "hi")?;
        let more = r.wait_for_event(Duration::from_millis(750), is_update_under);
        if more.iter().any(is_update_under) {
            return Ok(());
        }
        all_seen.extend(more);
        attempt += 1;
    }
    fatalf!("expected update for a file inside {} (gave up after {:?}), got {:?}", what, total, to_want_events(&all_seen))
}

// watcher_test.go:907
#[test]
fn watch_file_rename_existing() {
    run_for_each_watcher("TestWatchFileRenameExisting", |t, w| {
        let dir = new_tmp_dir(t);
        // Existing file present at subscribe time.
        let f1 = sub_path(&dir);
        write(&f1, "hi")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f2 = sub_path(&dir);
        std::fs::rename(&f1, &f2).map_err(|e| e.to_string())?;
        expect_event_set(&r, vec![want(EventKind::Delete, &f1), want(EventKind::Update, &f2)])?;
        Ok(())
    });
}

// watcher_test.go:2394
#[test]
fn subscribe_nested_dir_deletion_cleans_descendants() {
    run_for_each_watcher("TestSubscribeNestedDirDeletionCleansDescendants", |t, w| {
        let dir = new_tmp_dir(t);
        let sub = join(&dir, "parent");
        let nested = join(&sub, "child");
        std::fs::create_dir_all(&nested).map_err(|e| e.to_string())?;
        write(&join(&nested, "file.txt"), "x")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        std::fs::remove_dir_all(&sub).map_err(|e| e.to_string())?;
        expect_contains(&r, EventKind::Delete, &sub)?;
        Ok(())
    });
}

// watcher_test.go:2434
#[test]
fn non_recursive_file_update() {
    run_for_each_watcher("TestNonRecursiveFileUpdate", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for_opts(t, &dir, w, vec![])?;
        let f = sub_path(&dir);
        write(&f, "v1")?;
        let _ = r.wait_for_event(r.deadline(), |_| true); // consume create
        write(&f, "v2-longer")?;
        expect_event_sequence(&r, &[want(EventKind::Update, &f)])?;
        Ok(())
    });
}

// watcher_test.go:2452
#[test]
fn non_recursive_file_delete() {
    run_for_each_watcher("TestNonRecursiveFileDelete", |t, w| {
        let dir = new_tmp_dir(t);
        let f = sub_path(&dir);
        write(&f, "x")?;
        let (r, _) = subscribe_for_opts(t, &dir, w, vec![])?;
        std::fs::remove_file(&f).map_err(|e| e.to_string())?;
        expect_event_sequence(&r, &[want(EventKind::Delete, &f)])?;
        Ok(())
    });
}

// watcher_test.go:2469
#[test]
fn non_recursive_dir_create() {
    run_for_each_watcher("TestNonRecursiveDirCreate", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for_opts(t, &dir, w, vec![])?;
        let sub = sub_path(&dir);
        mkdir(&sub)?;
        expect_contains(&r, EventKind::Update, &sub)?;
        Ok(())
    });
}

// watcher_test.go:2513
#[test]
fn non_recursive_new_subdir_content_ignored() {
    run_for_each_watcher("TestNonRecursiveNewSubdirContentIgnored", |t, w| {
        let dir = new_tmp_dir(t);
        let (r, _) = subscribe_for_opts(t, &dir, w, vec![])?;
        // Create a new subdirectory and wait for the dir create event.
        let sub = sub_path(&dir);
        mkdir(&sub)?;
        expect_contains(&r, EventKind::Update, &sub)?;
        // Write a file inside the new subdirectory.
        let grandchild = sub_path(&sub);
        write(&grandchild, "nested")?;
        let marker = sub_path(&dir);
        write(&marker, "flush")?;
        // Should NOT see the grandchild event.
        let mut got = expect_contains(&r, EventKind::Update, &marker)?;
        got.extend(r.drain_quiet(2 * maxWaitTime));
        assert_no_events_for_path(&got, &grandchild, "expected no events for nested file")
    });
}

// watcher_test.go:2545
#[test]
fn non_recursive_and_recursive_same_dir() {
    run_for_each_watcher("TestNonRecursiveAndRecursiveSameDir", |t, w| {
        let dir = new_tmp_dir(t);
        let sub = join(&dir, "child");
        mkdir(&sub)?;
        let (r_non_rec, _) = subscribe_for_opts(t, &dir, w, vec![])?;
        let (r_rec, _) = subscribe_for_opts(t, &dir, w, vec![WatchOption::Recursive])?;
        // Create a grandchild file.
        let grandchild = sub_path(&sub);
        write(&grandchild, "deep")?;
        let marker = sub_path(&dir);
        write(&marker, "flush")?;
        // Recursive should see the grandchild.
        expect_contains(&r_rec, EventKind::Update, &grandchild)?;
        // Non-recursive should NOT see the grandchild.
        let mut got_non_rec = expect_contains(&r_non_rec, EventKind::Update, &marker)?;
        got_non_rec.extend(r_non_rec.drain_quiet(2 * maxWaitTime));
        assert_no_events_for_path(&got_non_rec, &grandchild, "non-recursive: expected no events for")
    });
}

// watcher_test.go:2623
#[test]
fn file_watch_update() {
    run_for_each_watcher("TestFileWatchUpdate", |t, w| {
        let dir = new_tmp_dir(t);
        let f = join(&dir, "target.txt");
        write(&f, "v1")?;
        let (r, _) = subscribe_file_for(t, &f, w)?;
        write(&f, "v2-longer")?;
        expect_event_sequence(&r, &[want(EventKind::Update, &f)])?;
        Ok(())
    });
}

// watcher_test.go:2682
#[test]
fn file_watch_multiple_same_dir() {
    run_for_each_watcher("TestFileWatchMultipleSameDir", |t, w| {
        let dir = new_tmp_dir(t);
        let f1 = join(&dir, "a.txt");
        let f2 = join(&dir, "b.txt");
        let (r1, _) = subscribe_file_for(t, &f1, w)?;
        let (r2, _) = subscribe_file_for(t, &f2, w)?;
        // Write to f1; only r1 should see it.
        write(&f1, "hello")?;
        assert_event_sequence(&r1.next(r1.deadline()), &[want(EventKind::Update, &f1)])?;
        expect_no_buffered_events(&r2, "r2 should not see f1 events")?;
        // Write to f2; only r2 should see it.
        write(&f2, "world")?;
        assert_event_sequence(&r2.next(r2.deadline()), &[want(EventKind::Update, &f2)])?;
        expect_no_buffered_events(&r1, "r1 should not see f2 events")
    });
}

// watcher_test.go:2714
#[test]
fn file_watch_delete_and_recreate() {
    run_for_each_watcher("TestFileWatchDeleteAndRecreate", |t, w| {
        let dir = new_tmp_dir(t);
        let f = join(&dir, "config.json");
        write(&f, r#"{"v":1}"#)?;
        let (r, _) = subscribe_file_for(t, &f, w)?;
        // Delete the file.
        std::fs::remove_file(&f).map_err(|e| e.to_string())?;
        expect_event_sequence(&r, &[want(EventKind::Delete, &f)])?;
        // Recreate it.
        write(&f, r#"{"v":2}"#)?;
        expect_contains(&r, EventKind::Update, &f)?;
        Ok(())
    });
}

// watcher_test.go:2738
#[test]
fn file_watch_non_existent_target() {
    run_for_each_watcher("TestFileWatchNonExistentTarget", |t, w| {
        let dir = new_tmp_dir(t);
        let f = join(&dir, "doesnotexist.txt");
        // File doesn't exist; subscribe should still succeed (watches the parent dir).
        let (r, _) = subscribe_file_for(t, &f, w)?;
        write(&f, "appeared")?;
        expect_event_sequence(&r, &[want(EventKind::Update, &f)])?;
        Ok(())
    });
}

// watcher_test.go:2758
#[test]
fn recursive_move_in_pre_populated() {
    run_for_each_watcher("TestRecursiveMoveInPrePopulated", |t, w| {
        let dir = new_tmp_dir(t);
        let outside = new_tmp_dir(t);
        // Build a tree outside the watched directory.
        std::fs::create_dir_all(join(&join(&join(&outside, "a"), "b"), "c")).map_err(|e| e.to_string())?;
        let (r, _) = subscribe_for_opts(t, &dir, w, vec![WatchOption::Recursive])?;
        // Move the pre-populated tree into the watched directory, and consume the move-in events.
        let dest = join(&dir, "tree");
        std::fs::rename(&outside, &dest).map_err(|e| e.to_string())?;
        let _ = r.drain_quiet(Duration::from_millis(500));
        // Now modify a file deep inside the moved tree.
        let nested_dir = join(&join(&join(&dest, "a"), "b"), "c");
        nudge_until_update_under(&r, &nested_dir, "deep", r.deadline(), "moved-in tree")
    });
}

// watcher_test.go:2839
#[test]
fn atomic_save_file_watch() {
    run_for_each_watcher("TestAtomicSaveFileWatch", |t, w| {
        let dir = new_tmp_dir(t);
        let target = join(&dir, "target.txt");
        write(&target, "v1")?;
        let (r, _) = subscribe_file_for(t, &target, w)?;
        let tmp = format!("{target}.tmp");
        write(&tmp, "v2")?;
        std::fs::rename(&tmp, &target).map_err(|e| e.to_string())?;
        let got = r.wait_for_event(r.deadline(), |e| e.path == target);
        if filter_events_for_paths(&got, &[&target]).is_empty() {
            fatalf!("expected events for {} after atomic save, got none", target);
        }
        Ok(())
    });
}

// watcher_test.go:2868
#[test]
fn replace_dir_with_file() {
    run_for_each_watcher("TestReplaceDirWithFile", |t, w| {
        let dir = new_tmp_dir(t);
        let child = join(&dir, "child");
        mkdir(&child)?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        std::fs::remove_dir(&child).map_err(|e| e.to_string())?;
        write(&child, "now a file")?;
        let got = r.wait_for_event(r.deadline(), |e| e.path == child);
        if filter_events_for_paths(&got, &[&child]).is_empty() {
            fatalf!("expected events for dir->file replacement at {}, got none", child);
        }
        Ok(())
    });
}

// watcher_test.go:2897
#[test]
fn recreate_subdir_and_modify() {
    run_for_each_watcher("TestRecreateSubdirAndModify", |t, w| {
        let dir = new_tmp_dir(t);
        let sub = join(&dir, "sub");
        mkdir(&sub)?;
        write(&join(&sub, "file.txt"), "v1")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        // Delete the subdirectory tree, then recreate the same path.
        std::fs::remove_dir_all(&sub).map_err(|e| e.to_string())?;
        let _ = r.drain_quiet(Duration::from_millis(500));
        mkdir(&sub)?;
        // Give the backend a chance to observe the create and arm its watch on the new directory before writing in it.
        std::thread::sleep(Duration::from_millis(150));
        nudge_until_update_under(&r, &sub, "attempt", r.deadline() * 2, "recreated sub")
    });
}

// watcher_test.go:2961
#[test]
fn replace_parent_dir_with_different() {
    run_for_each_watcher("TestReplaceParentDirWithDifferent", |t, w| {
        let dir = new_tmp_dir(t);
        let sub = join(&dir, "pkg");
        std::fs::create_dir_all(join(&sub, "old")).map_err(|e| e.to_string())?;
        write(&join(&join(&sub, "old"), "a.txt"), "a")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        // Replace: remove old tree, create new tree at same path.
        std::fs::remove_dir_all(&sub).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(join(&sub, "new")).map_err(|e| e.to_string())?;
        write(&join(&join(&sub, "new"), "b.txt"), "b")?;
        let _ = r.drain_quiet(Duration::from_millis(500));
        // Now modify something inside the replaced tree.
        nudge_until_update_under(&r, &join(&sub, "new"), "attempt", r.deadline(), "replaced tree")
    });
}

// watcher_test.go:3016
#[test]
fn round_trip_rename() {
    run_for_each_watcher("TestRoundTripRename", |t, w| {
        if w.name() == "kqueue" {
            // kqueue fd-based tracking delivers a stale delete before the parent's write reconciles
            eprintln!("TestRoundTripRename/kqueue: SKIPPED");
            return Ok(());
        }
        let dir = new_tmp_dir(t);
        let orig = join(&dir, "data.txt");
        write(&orig, "content")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let tmp = join(&dir, "data.txt.bak");
        std::fs::rename(&orig, &tmp).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &orig).map_err(|e| e.to_string())?;
        // Either some events or zero events (coalesced to no-op) are both acceptable, as long as a delete is
        // followed by an update: the watcher recovered.
        let got = r.gather_until_quiet(r.deadline(), Duration::from_millis(500));
        let got = filter_events_for_paths(&got, &[&orig]);
        let has_delete = contains_event(&got, EventKind::Delete, &orig);
        let has_update = contains_event(&got, EventKind::Update, &orig);
        if has_delete && !has_update {
            fatalf!(
                "round-trip rename left a stale delete without recovery for {}; events: {:?}",
                orig,
                to_want_events(&got)
            );
        }
        Ok(())
    });
}

// watcher_test.go:2291
#[test]
fn rename_dir_out_of_tree_no_stale_events() {
    run_for_each_watcher("TestRenameDirOutOfTreeNoStaleEvents", |t, w| {
        let watched = new_tmp_dir(t);
        let outside = new_tmp_dir(t); // separate directory, NOT watched.
        // Build a nested subtree under sub/. The descendant subdirectories are the ones that exercise the bug: a
        // backend that cleans `sub` by exact match leaves `sub/inner` with a stale path, and later changes to files
        // at the new location would then surface against the old one.
        let sub = join(&watched, "sub");
        let inner = join(&sub, "inner");
        std::fs::create_dir_all(&inner).map_err(|e| e.to_string())?;
        write(&join(&inner, "leaf.txt"), "v1")?;
        let (r, _) = subscribe_for_opts(t, &watched, w, vec![WatchOption::Recursive])?;
        // Rename the whole subtree out of the watched dir and consume the rename-away events.
        let dest = join(&outside, "moved");
        std::fs::rename(&sub, &dest).map_err(|e| e.to_string())?;
        let _ = r.drain_quiet(Duration::from_millis(500));
        // Modify the file at its new location.
        write(&join(&join(&dest, "inner"), "leaf.txt"), "v2-longer")?;
        let extra = r.drain_quiet(Duration::from_millis(800));
        // Allow events for unrelated parts of the watched dir, but none at or under the moved subtree.
        let old_prefix = format!("{sub}{}", char::from(crate::filepath::HOST.separator()));
        for e in &extra {
            if e.path == sub || e.path.starts_with(&old_prefix) {
                fatalf!("stale event for moved-out path {}: {:?}\nall extras: {:?}", e.path, e, to_want_events(&extra));
            }
        }
        Ok(())
    });
}

// Links `link` to the file `target`; false when the platform denies it (Windows needs a privilege or developer mode).
fn make_file_symlink(target: &str, link: &str) -> bool {
    #[cfg(windows)]
    return std::os::windows::fs::symlink_file(target, link).is_ok();
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link).is_ok();
}

// watcher_test.go:1212
#[test]
fn subscribe_symlink_create() {
    run_for_each_watcher("TestSubscribeSymlinkCreate", |t, w| {
        let dir = new_tmp_dir(t);
        let f1 = sub_path(&dir);
        write(&f1, "x")?;
        let (r, _) = subscribe_for(t, &dir, w)?;
        let f2 = sub_path(&dir);
        if !make_file_symlink(&f1, &f2) {
            eprintln!("TestSubscribeSymlinkCreate/{}: SKIPPED, cannot create symlinks here", w.name());
            return Ok(());
        }
        expect_event_sequence(&r, &[want(EventKind::Update, &f2)])?;
        Ok(())
    });
}

// watcher_test.go:1232
#[test]
fn subscribe_symlink_delete() {
    run_for_each_watcher("TestSubscribeSymlinkDelete", |t, w| {
        let dir = new_tmp_dir(t);
        let f1 = sub_path(&dir);
        let f2 = sub_path(&dir);
        write(&f1, "x")?;
        if !make_file_symlink(&f1, &f2) {
            eprintln!("TestSubscribeSymlinkDelete/{}: SKIPPED, cannot create symlinks here", w.name());
            return Ok(());
        }
        let (r, _) = subscribe_for(t, &dir, w)?;
        std::fs::remove_file(&f2).map_err(|e| e.to_string())?;
        expect_event_sequence(&r, &[want(EventKind::Delete, &f2)])?;
        Ok(())
    });
}

// watcher_test.go:1252
#[test]
fn subscribe_symlinked_directory_rebases_target_events() {
    run_for_each_watcher("TestSubscribeSymlinkedDirectoryRebasesTargetEvents", |t, w| {
        let dir = new_tmp_dir(t);
        let target = join(&dir, "target");
        mkdir(&target)?;
        let link = join(&dir, "link");
        make_dir_symlink(&target, &link);
        let (r, _) = subscribe_for(t, &link, w)?;
        write(&join(&target, "child"), "x")?;
        expect_contains(&r, EventKind::Update, &join(&link, "child"))?;
        Ok(())
    });
}

// watcher_test.go:1493
#[test]
fn watch_directories_batch() {
    run_for_each_watcher("TestWatchDirectoriesBatch", |t, w| {
        let dir1 = new_tmp_dir(t);
        let dir2 = new_tmp_dir(t);
        let r1 = new_recorder(t, Some(w));
        let r2 = new_recorder(t, Some(w));
        let request = |dir: &str, r: &Arc<recordingWatcher>| WatchDirectoryRequest {
            dir: dir.to_string(),
            callback: Some(r.callback()),
            options: vec![WatchOption::Recursive],
        };
        let watches = w
            .watch_directories(vec![request(&dir1, &r1), request(&dir2, &r2)])
            .map_err(|e| e.to_string())?;
        t.cleanup(move || {
            for watch in &watches {
                let _ = watch.close();
            }
        });
        std::thread::sleep(settle_sleep(w));
        let f1 = sub_path(&dir1);
        let f2 = sub_path(&dir2);
        write(&f1, "a")?;
        write(&f2, "b")?;
        assert_event_sequence(&r1.next(r1.deadline()), &[want(EventKind::Update, &f1)])?;
        assert_event_sequence(&r2.next(r2.deadline()), &[want(EventKind::Update, &f2)])
    });
}

// watcher_test.go:1836
#[test]
fn subscribe_rejects_nil_callback() {
    run_for_each_watcher("TestSubscribeRejectsNilCallback", |t, w| {
        let request = WatchDirectoryRequest { dir: new_tmp_dir(t), callback: None, options: vec![] };
        if w.watch_directories(vec![request]).is_ok() {
            fatalf!("WatchDirectory(nil callback) should return an error");
        }
        Ok(())
    });
}

// watcher_test.go:2066
#[test]
fn subscribe_concurrent_subscribe_unsubscribe() {
    run_for_each_watcher("TestSubscribeConcurrentSubscribeUnsubscribe", |t, w| {
        let dir = new_tmp_dir(t);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    if let Ok(sub) = w.watch_directory(&dir, new_recorder(&TestT::new(1), None).callback(), vec![]) {
                        let _ = sub.close();
                    }
                });
            }
        });
        Ok(())
    });
}

// watcher_test.go:2223
#[test]
fn file_callback_forwards_err_alongside_events() {
    let dir = native("/abs/dir");
    let target = native("/abs/dir/target.txt");
    let other = native("/abs/dir/sibling.txt");
    let got: Arc<Mutex<Vec<(Vec<Event>, Option<Error>)>>> = Arc::default();
    let dw = dirWatch::new(dir.clone(), dir.clone(), &debounce::new(), pathComparer::default(), None, true);
    let sink = Arc::clone(&got);
    let cb: WatchCallback = Arc::new(move |events: Vec<Event>, err: Option<Error>| {
        sink.lock().unwrap().push((events, err));
    });
    dw.add_callback(&dir, &dir, false, cb, None, &target);
    let deliver = |events: Vec<Event>, err: Option<Error>| {
        for e in &events {
            if e.kind == EventKind::Delete {
                dw.events.remove(&e.path);
            } else {
                dw.events.update(&e.path);
            }
        }
        if let Some(err) = err {
            dw.events.set_error(err);
        }
        dw.trigger_callbacks_for_test();
    };
    let overflow: Error = ErrOverflow.into();

    // Plain events: only target events pass through, sibling dropped.
    deliver(vec![Event::new(EventKind::Update, &*target), Event::new(EventKind::Update, &*other)], None);
    {
        let calls = got.lock().unwrap();
        assert!(
            calls.len() == 1 && calls[0].0.len() == 1 && calls[0].0[0].path == target && calls[0].1.is_none(),
            "plain delivery: got {calls:?}"
        );
    }

    // Err only, no matching events: still forwarded with empty slice.
    got.lock().unwrap().clear();
    deliver(vec![Event::new(EventKind::Update, &*other)], Some(overflow.clone()));
    {
        let calls = got.lock().unwrap();
        assert!(
            calls.len() == 1 && calls[0].0.is_empty() && calls[0].1.as_ref().is_some_and(|e| e.is(ErrOverflow)),
            "err-only delivery: got {calls:?}"
        );
    }

    // Err with matching events: deliver both the filtered events and err.
    got.lock().unwrap().clear();
    deliver(
        vec![Event::new(EventKind::Delete, &*target), Event::new(EventKind::Update, &*other)],
        Some(overflow),
    );
    {
        let calls = got.lock().unwrap();
        assert!(
            calls.len() == 1
                && calls[0].0.len() == 1
                && calls[0].0[0].path == target
                && calls[0].0[0].kind == EventKind::Delete
                && calls[0].1.as_ref().is_some_and(|e| e.is(ErrOverflow)),
            "combined delivery: got {calls:?}"
        );
    }

    // No events, no err: callback not invoked at all.
    got.lock().unwrap().clear();
    deliver(vec![], None);
    assert!(got.lock().unwrap().is_empty(), "no-op delivery: got {:?}", got.lock().unwrap());
    dw.destroy_debounce_for_test();
}

// watcher_test.go:1788
#[test]
fn recursive_watch_with_ignore_does_not_filter_by_logical_root() {
    let mut t = TestT::new(1);
    let dir = join(&t.temp_dir(), "root");
    let dw = dirWatch::new(dir.clone(), dir.clone(), &debounce::new(), pathComparer::default(), None, true);
    let got: Arc<Mutex<Vec<Event>>> = Arc::default();
    let outside_path = join(&join(&join(&t.temp_dir(), "outside"), "pkg"), "index.ts");
    let sink = Arc::clone(&got);
    let cb: WatchCallback = Arc::new(move |events: Vec<Event>, err: Option<Error>| {
        assert!(err.is_none(), "unexpected error: {err:?}");
        sink.lock().unwrap().extend(events);
    });
    dw.add_callback(&dir, &dir, true, cb, Some(Arc::new(|_: &str| false)), "");
    dw.events.update(&outside_path);
    dw.trigger_callbacks_for_test();
    dw.destroy_debounce_for_test();
    let got = got.lock().unwrap();
    assert_eq!(to_want_events(&got), vec![want(EventKind::Update, &outside_path)]);
}

// tsrs-only (`eventIndex`): with several callbacks, each gets the events it gets when it is the only one (one
// callback is delivered without the index).
#[test]
fn indexed_delivery_matches_single_callback_delivery() {
    let dirs = [
        "/root",
        "/root/a",
        "/root/A",
        "/root/a/b",
        "/root/x",
        "/root/\u{017f}",
        "/root/s",
        "/root/\u{00df}",
    ]
    .map(native);
    let paths = [
        "/root",
        "/root/f.ts",
        "/root/F.ts",
        "/root/a",
        "/root/a/f.ts",
        "/root/A/G.ts",
        "/root/a/b/h.ts",
        "/root/a/b/c/i.ts",
        "/root/x/j.ts",
        "/root/\u{017f}/k.ts",
        "/root/S/l.ts",
        "/root/s/m.ts",
        "/root/SS/n.ts",
        "/root/\u{00df}/o.ts",
        "/root/a/B",
        "/root/ab/p.ts",
        "/other/q.ts",
    ]
    .map(native);
    let root = native("/root");
    for ignore_case in [false, true] {
        for physical in ["/root", "/phys"].map(native) {
            let physical = physical.as_str();
            // (dir, physical dir, recursive, file)
            let mut configs: Vec<(String, String, bool, String)> = Vec::new();
            for dir in &dirs {
                for recursive in [false, true] {
                    configs.push((
                        dir.clone(),
                        rebase_path(dir, &root, physical),
                        recursive,
                        String::new(),
                    ));
                }
            }
            let root_a = native("/root/a");
            configs.push((
                root_a.clone(),
                rebase_path(&root_a, &root, physical),
                false,
                native("/root/a/f.ts"),
            ));
            configs.push((root.clone(), physical.to_string(), false, native("/root/f.ts")));
            // A callback under another name for the same physical directory (a symlink to it).
            for (dir, sub) in [("/link/a", "/a"), ("/link", ""), ("/LINK/A/B", "/a/b")] {
                configs.push((
                    native(dir),
                    format!("{physical}{}", host_separators(sub)),
                    false,
                    String::new(),
                ));
            }
            let deliver = |configs: &[(String, String, bool, String)]| -> Vec<Vec<String>> {
                let dw = dirWatch::new(
                    root.clone(),
                    physical.to_string(),
                    &debounce::new(),
                    pathComparer { ignore_case },
                    None,
                    true,
                );
                let got: Vec<Arc<Mutex<Vec<String>>>> =
                    configs.iter().map(|_| Arc::default()).collect();
                for ((dir, physical_dir, recursive, file), sink) in configs.iter().zip(&got) {
                    let sink = Arc::clone(sink);
                    let cb: WatchCallback =
                        Arc::new(move |events: Vec<Event>, _: Option<Error>| {
                            sink.lock().unwrap().extend(
                                events.into_iter().map(|e| format!("{} {}", e.kind, e.path)),
                            );
                        });
                    dw.add_callback(dir, physical_dir, *recursive, cb, None, file);
                }
                for path in &paths {
                    dw.events.update(path);
                }
                dw.events.remove(&native("/root/a/b/gone.ts"));
                dw.trigger_callbacks_for_test();
                dw.destroy_debounce_for_test();
                got.iter()
                    .map(|g| {
                        let mut g = g.lock().unwrap().clone();
                        g.sort();
                        g
                    })
                    .collect()
            };
            let together = deliver(&configs);
            for (i, config) in configs.iter().enumerate() {
                let alone = deliver(std::slice::from_ref(config));
                assert_eq!(
                    together[i], alone[0],
                    "ignore_case={ignore_case} physical={physical} callback {config:?}"
                );
            }
        }
    }
}
