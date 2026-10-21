// Go nativepath.Realpath, which follows what Node's fs.realpath.native does.

use std::io;

pub(crate) fn realpath(path: &str) -> io::Result<String> {
    let resolved = std::fs::canonicalize(path)?.to_string_lossy().into_owned();
    Ok(if cfg!(windows) { strip_verbatim_prefix(resolved) } else { resolved })
}

// Windows canonicalization returns `\\?\C:\x` and `\\?\UNC\host\share\x`; callers pass `C:\x` and `\\host\share\x`.
// Other verbatim forms (`\\?\Volume{...}`) have no shorter spelling and stay as they are.
fn strip_verbatim_prefix(path: String) -> String {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::strip_verbatim_prefix;

    #[test]
    fn strips_the_verbatim_prefix() {
        let cases = [
            (r"\\?\C:\a\b", r"C:\a\b"),
            (r"\\?\UNC\host\share\a", r"\\host\share\a"),
            (r"C:\a\b", r"C:\a\b"),
            (r"\\host\share\a", r"\\host\share\a"),
            (r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\a", r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\a"),
            ("/a/b", "/a/b"),
        ];
        for (path, want) in cases {
            assert_eq!(strip_verbatim_prefix(path.to_string()), want, "{path:?}");
        }
    }
}
