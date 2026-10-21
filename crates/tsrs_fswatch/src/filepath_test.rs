// Go path/filepath on both operating systems. The Windows rows are Go 1.27.1's own answers (`filepath.Clean`,
// `Dir`, `VolumeName` and `IsAbs` run on Windows); the Unix rows are the documented examples.

use crate::filepath::Os;

// (path, Clean, Dir, VolumeName, IsAbs)
const WINDOWS_CASES: &[(&str, &str, &str, &str, bool)] = &[
    ("", ".", ".", "", false),
    (".", ".", ".", "", false),
    ("..", "..", ".", "", false),
    ("C:", "C:.", "C:.", "C:", false),
    ("C:\\", "C:\\", "C:\\", "C:", true),
    ("C:/", "C:\\", "C:\\", "C:", true),
    ("C:.", "C:.", "C:.", "C:", false),
    ("C:a", "C:a", "C:.", "C:", false),
    ("C:a\\b", "C:a\\b", "C:a", "C:", false),
    ("c:/a/b", "c:\\a\\b", "c:\\a", "c:", true),
    ("C:/a/b", "C:\\a\\b", "C:\\a", "C:", true),
    ("C:/a//b/", "C:\\a\\b", "C:\\a\\b", "C:", true),
    ("C:/a/./b", "C:\\a\\b", "C:\\a", "C:", true),
    ("C:/a/../b", "C:\\b", "C:\\", "C:", true),
    ("C:/a/..", "C:\\", "C:\\a", "C:", true),
    ("C:/..", "C:\\", "C:\\", "C:", true),
    ("C:/../a", "C:\\a", "C:\\", "C:", true),
    ("C:\\a\\..\\..\\b", "C:\\b", "C:\\", "C:", true),
    ("C:/a/b/", "C:\\a\\b", "C:\\a\\b", "C:", true),
    ("C:\\a", "C:\\a", "C:\\", "C:", true),
    ("C:\\a\\b\\c", "C:\\a\\b\\c", "C:\\a\\b", "C:", true),
    ("a", "a", ".", "", false),
    ("a/b", "a\\b", "a", "", false),
    ("a\\b", "a\\b", "a", "", false),
    ("a/../b", "b", ".", "", false),
    ("../a", "..\\a", "..", "", false),
    ("a/../..", "..", ".", "", false),
    ("./a", "a", ".", "", false),
    ("a/.", "a", "a", "", false),
    ("/", "\\", "\\", "", false),
    ("\\", "\\", "\\", "", false),
    ("/a", "\\a", "\\", "", false),
    ("\\a", "\\a", "\\", "", false),
    ("/a/b", "\\a\\b", "\\a", "", false),
    ("\\a\\b\\", "\\a\\b", "\\a\\b", "", false),
    ("/a/../..", "\\", "\\", "", false),
    ("\\\\host\\share", "\\\\host\\share", "\\\\host\\share", "\\\\host\\share", true),
    ("\\\\host\\share\\", "\\\\host\\share\\", "\\\\host\\share\\", "\\\\host\\share", true),
    ("\\\\host\\share\\a", "\\\\host\\share\\a", "\\\\host\\share\\", "\\\\host\\share", true),
    ("\\\\host\\share\\a\\..", "\\\\host\\share\\", "\\\\host\\share\\a", "\\\\host\\share", true),
    ("\\\\host\\share\\..", "\\\\host\\share\\", "\\\\host\\share\\", "\\\\host\\share", true),
    ("//host/share/x", "\\\\host\\share\\x", "\\\\host\\share\\", "\\\\host\\share", true),
    ("//host/share/x/../y", "\\\\host\\share\\y", "\\\\host\\share\\", "\\\\host\\share", true),
    ("\\\\host", "\\\\host", "\\\\host", "\\\\host", true),
    ("\\\\", "\\\\", "\\\\.", "\\\\", true),
    (
        "C:/Users/Robert/AppData/Local/Temp/tsrs-fswatch-1-0",
        "C:\\Users\\Robert\\AppData\\Local\\Temp\\tsrs-fswatch-1-0",
        "C:\\Users\\Robert\\AppData\\Local\\Temp",
        "C:",
        true,
    ),
    (
        "C:/Users/Robert/AppData/Local/Temp/x/../y/./z",
        "C:\\Users\\Robert\\AppData\\Local\\Temp\\y\\z",
        "C:\\Users\\Robert\\AppData\\Local\\Temp\\y",
        "C:",
        true,
    ),
    ("C:/a b/c d", "C:\\a b\\c d", "C:\\a b", "C:", true),
    ("C:/a/b.txt", "C:\\a\\b.txt", "C:\\a", "C:", true),
    ("C:/a/.hidden", "C:\\a\\.hidden", "C:\\a", "C:", true),
    ("C:/a/...", "C:\\a\\...", "C:\\a", "C:", true),
    ("C:/a/b../c", "C:\\a\\b..\\c", "C:\\a\\b..", "C:", true),
    ("a/../c:", ".\\c:", ".", "", false),
    ("ab:c", "ab:c", ".", "", false),
    ("ab:c/../d", "d", ".", "", false),
    ("\\\\?\\C:\\a", "\\\\?\\C:\\a", "\\\\?\\C:\\", "\\\\?\\C:", true),
    ("\\\\?\\C:\\", "\\\\?\\C:\\", "\\\\?\\C:\\", "\\\\?\\C:", true),
    ("\\\\?\\C:", "\\\\?\\C:", "\\\\?\\C:", "\\\\?\\C:", true),
    (
        "\\\\.\\UNC\\host\\share\\x",
        "\\\\.\\UNC\\host\\share\\x",
        "\\\\.\\UNC\\host\\share\\",
        "\\\\.\\UNC\\host\\share",
        true,
    ),
    ("\\??\\C:\\x", "\\??\\C:\\x", "\\??\\C:\\", "\\??\\C:", true),
    ("\\a\\..\\??\\c:\\x", "\\.\\??\\c:\\x", "\\.\\??\\c:", "", false),
    ("\\\\?\\", "\\\\?\\", "\\\\?\\", "\\\\?\\", true),
    ("\\\\?", "\\\\?", "\\\\?", "\\\\?", true),
    ("C:/é/ü", "C:\\é\\ü", "C:\\é", "C:", true),
    ("C:\\é\\..\\ü", "C:\\ü", "C:\\", "C:", true),
];

// (path, Clean, Dir, IsAbs)
const UNIX_CASES: &[(&str, &str, &str, bool)] = &[
    ("", ".", ".", false),
    (".", ".", ".", false),
    ("..", "..", ".", false),
    ("/", "/", "/", true),
    ("//", "/", "/", true),
    ("/a", "/a", "/", true),
    ("/a/b", "/a/b", "/a", true),
    ("/a//b/", "/a/b", "/a/b", true),
    ("/a/./b", "/a/b", "/a", true),
    ("/a/../b", "/b", "/", true),
    ("/..", "/", "/", true),
    ("/../a", "/a", "/", true),
    ("a", "a", ".", false),
    ("a/b", "a/b", "a", false),
    ("a/../b", "b", ".", false),
    ("../a", "../a", "..", false),
    ("a/../..", "..", ".", false),
    ("C:\\a", "C:\\a", ".", false),
    ("/é/ü", "/é/ü", "/é", true),
];

#[test]
fn test_windows_clean() {
    for &(path, clean, ..) in WINDOWS_CASES {
        assert_eq!(Os::Windows.clean(path), clean, "Clean({path:?})");
    }
}

#[test]
fn test_windows_dir() {
    for &(path, _, dir, ..) in WINDOWS_CASES {
        assert_eq!(Os::Windows.dir(path), dir, "Dir({path:?})");
    }
}

#[test]
fn test_windows_volume_name() {
    for &(path, _, _, volume, _) in WINDOWS_CASES {
        assert_eq!(Os::Windows.volume_name(path), *volume, "VolumeName({path:?})");
    }
}

#[test]
fn test_windows_is_abs() {
    for &(path, _, _, _, is_abs) in WINDOWS_CASES {
        assert_eq!(Os::Windows.is_abs(path), is_abs, "IsAbs({path:?})");
    }
}

#[test]
fn test_unix_clean_dir_is_abs() {
    for &(path, clean, dir, is_abs) in UNIX_CASES {
        assert_eq!(Os::Unix.clean(path), clean, "Clean({path:?})");
        assert_eq!(Os::Unix.dir(path), dir, "Dir({path:?})");
        assert_eq!(Os::Unix.is_abs(path), is_abs, "IsAbs({path:?})");
        assert_eq!(Os::Unix.volume_name(path), "", "VolumeName({path:?})");
    }
}

#[test]
fn test_path_separators() {
    assert_eq!(Os::Windows.separator(), b'\\');
    assert!(Os::Windows.is_path_separator(b'/') && Os::Windows.is_path_separator(b'\\'));
    assert_eq!(Os::Unix.separator(), b'/');
    assert!(Os::Unix.is_path_separator(b'/') && !Os::Unix.is_path_separator(b'\\'));
}
