// Go path/filepath (internal/filepathlite): the parts watcher.go uses, for either operating system's path rules.

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Os {
    Unix,
    Windows,
}

pub(crate) const HOST: Os = if cfg!(windows) { Os::Windows } else { Os::Unix };

// A lazily constructed path buffer: it holds no bytes of its own until the output differs from the input.
struct LazyBuf<'a> {
    path: &'a [u8],
    buf: Option<Vec<u8>>,
    w: usize,
}

impl LazyBuf<'_> {
    fn index(&self, i: usize) -> u8 {
        match &self.buf {
            Some(buf) => buf[i],
            None => self.path[i],
        }
    }

    fn append(&mut self, c: u8) {
        if self.buf.is_none() {
            if self.path.get(self.w) == Some(&c) {
                self.w += 1;
                return;
            }
            let mut buf = vec![0; self.path.len()];
            buf[..self.w].copy_from_slice(&self.path[..self.w]);
            self.buf = Some(buf);
        }
        if let Some(buf) = &mut self.buf {
            if self.w == buf.len() {
                buf.push(c);
            } else {
                buf[self.w] = c;
            }
        }
        self.w += 1;
    }

    fn prepend(&mut self, prefix: &[u8]) {
        let buf = self.buf.get_or_insert_with(Vec::new);
        buf.splice(0..0, prefix.iter().copied());
        self.w += prefix.len();
    }

    fn bytes(&self) -> &[u8] {
        match &self.buf {
            Some(buf) => &buf[..self.w],
            None => &self.path[..self.w],
        }
    }
}

impl Os {
    // filepathlite Separator
    pub(crate) fn separator(self) -> u8 {
        match self {
            Os::Unix => b'/',
            Os::Windows => b'\\',
        }
    }

    // filepathlite IsPathSeparator
    pub(crate) fn is_path_separator(self, c: u8) -> bool {
        c == b'/' || (self == Os::Windows && c == b'\\')
    }

    // filepathlite FromSlash
    fn native_separators(self, path: &str) -> String {
        match self {
            Os::Unix => path.to_string(),
            Os::Windows => path.replace('/', "\\"),
        }
    }

    // filepathlite VolumeName
    pub(crate) fn volume_name(self, path: &str) -> String {
        self.native_separators(&path[..self.volume_name_len(path)])
    }

    // filepathlite IsAbs
    pub(crate) fn is_abs(self, path: &str) -> bool {
        let bytes = path.as_bytes();
        if self == Os::Unix {
            return bytes.first() == Some(&b'/');
        }
        let vol_len = self.volume_name_len(path);
        if vol_len == 0 {
            return false;
        }
        // If the volume name starts with a double slash, this is an absolute path.
        if self.is_path_separator(bytes[0]) && self.is_path_separator(bytes[1]) {
            return true;
        }
        bytes.get(vol_len).is_some_and(|&c| self.is_path_separator(c))
    }

    // filepathlite volumeNameLen: the length of the leading volume name on Windows, 0 elsewhere.
    fn volume_name_len(self, path: &str) -> usize {
        if self == Os::Unix {
            return 0;
        }
        let bytes = path.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' {
            // Path starts with a drive letter.
            return 2;
        }
        if bytes.is_empty() || !self.is_path_separator(bytes[0]) {
            // Path does not have a volume component.
            return 0;
        }
        if self.path_has_prefix_fold(bytes, br"\\.")
            || self.path_has_prefix_fold(bytes, br"\\?")
            || self.path_has_prefix_fold(bytes, br"\??")
        {
            // Path starts with a device prefix: \\.\ for Local Device paths, or \\?\ or \??\ for Root Local Device
            // paths.
            if bytes.len() == 3 {
                return 3; // exactly \\., \\?, or \??
            }
            if self.path_has_prefix_fold(&bytes[4..], b"UNC") {
                return self.valid_volume_name_len(bytes, self.unc_len(bytes, br"\\.\UNC\".len()));
            }
            // The next component after the device prefix is part of the volume name, which means Clean(`\\?\c:\`)
            // won't remove the trailing \.
            return match self.cut_path(&bytes[4..]) {
                None => self.valid_volume_name_len(bytes, bytes.len()),
                Some((_, rest)) => {
                    self.valid_volume_name_len(bytes, bytes.len() - rest.len() - 1)
                }
            };
        }
        if bytes.len() >= 2 && self.is_path_separator(bytes[1]) {
            // Path starts with \\, and is a UNC path.
            return self.valid_volume_name_len(bytes, self.unc_len(bytes, 2));
        }
        0
    }

    // filepathlite validVolumeNameLen: n if path[:n] is a valid Windows volume name, 0 if it contains a ".." part.
    fn valid_volume_name_len(self, path: &[u8], n: usize) -> usize {
        let mut p = &path[..n];
        while !p.is_empty() {
            let (part, rest) = match self.cut_path(p) {
                Some(cut) => cut,
                None => (p, &p[p.len()..]),
            };
            if part == b".." {
                return 0;
            }
            p = rest;
        }
        n
    }

    // filepathlite pathHasPrefixFold: whether s begins with prefix, ignoring case and treating all path separators as
    // equivalent; if s is longer than prefix, s[len(prefix)] must be a path separator.
    fn path_has_prefix_fold(self, s: &[u8], prefix: &[u8]) -> bool {
        if s.len() < prefix.len() {
            return false;
        }
        for (&p, &c) in prefix.iter().zip(s) {
            if self.is_path_separator(p) {
                if !self.is_path_separator(c) {
                    return false;
                }
            } else if !p.eq_ignore_ascii_case(&c) {
                return false;
            }
        }
        s.len() == prefix.len() || self.is_path_separator(s[prefix.len()])
    }

    // filepathlite uncLen: the length of the volume prefix of a UNC path. prefix_len is the prefix before the host,
    // 2 for "//host/share".
    fn unc_len(self, path: &[u8], prefix_len: usize) -> usize {
        let mut count = 0;
        for (i, &c) in path.iter().enumerate().skip(prefix_len) {
            if self.is_path_separator(c) {
                count += 1;
                if count == 2 {
                    return i;
                }
            }
        }
        path.len()
    }

    // filepathlite cutPath: slices path around the first path separator.
    fn cut_path(self, path: &[u8]) -> Option<(&[u8], &[u8])> {
        let i = path.iter().position(|&c| self.is_path_separator(c))?;
        Some((&path[..i], &path[i + 1..]))
    }

    // filepathlite Clean
    pub(crate) fn clean(self, path: &str) -> String {
        let original = path.as_bytes();
        let vol_len = self.volume_name_len(path);
        let path = &original[vol_len..];
        if path.is_empty() {
            if vol_len > 1 && self.is_path_separator(original[0]) && self.is_path_separator(original[1]) {
                // should be UNC
                return self.native_separators(&String::from_utf8_lossy(original));
            }
            return format!("{}.", String::from_utf8_lossy(original));
        }
        let rooted = self.is_path_separator(path[0]);
        let sep = self.separator();

        // Invariants:
        //	reading from path; r is index of next byte to process.
        //	writing to buf; w is index of next byte to write.
        //	dotdot is index in buf where .. must stop, either because it is the leading slash or it is a leading
        //	../../.. prefix.
        let n = path.len();
        let mut out = LazyBuf { path, buf: None, w: 0 };
        let (mut r, mut dotdot) = (0, 0);
        if rooted {
            out.append(sep);
            (r, dotdot) = (1, 1);
        }

        while r < n {
            if self.is_path_separator(path[r]) {
                // empty path element
                r += 1;
            } else if path[r] == b'.' && (r + 1 == n || self.is_path_separator(path[r + 1])) {
                // . element
                r += 1;
            } else if path[r] == b'.'
                && path[r + 1] == b'.'
                && (r + 2 == n || self.is_path_separator(path[r + 2]))
            {
                // .. element: remove to last separator
                r += 2;
                if out.w > dotdot {
                    // can backtrack
                    out.w -= 1;
                    while out.w > dotdot && !self.is_path_separator(out.index(out.w)) {
                        out.w -= 1;
                    }
                } else if !rooted {
                    // cannot backtrack, but not rooted, so append .. element.
                    if out.w > 0 {
                        out.append(sep);
                    }
                    out.append(b'.');
                    out.append(b'.');
                    dotdot = out.w;
                }
            } else {
                // real path element.
                // add slash if needed
                if (rooted && out.w != 1) || (!rooted && out.w != 0) {
                    out.append(sep);
                }
                // copy element
                while r < n && !self.is_path_separator(path[r]) {
                    out.append(path[r]);
                    r += 1;
                }
            }
        }

        // Turn empty string into "."
        if out.w == 0 {
            out.append(b'.');
        }

        self.post_clean(&mut out, vol_len); // avoid creating absolute paths on Windows
        let mut cleaned = original[..vol_len].to_vec();
        cleaned.extend_from_slice(out.bytes());
        self.native_separators(&String::from_utf8_lossy(&cleaned))
    }

    // filepathlite postClean: adjusts the results of Clean to avoid turning a relative path into an absolute or
    // rooted one.
    fn post_clean(self, out: &mut LazyBuf, vol_len: usize) {
        if self == Os::Unix || vol_len != 0 {
            return;
        }
        let Some(buf) = &out.buf else {
            return;
        };
        let sep = self.separator();
        // If a ':' appears in the path element at the start of a path, insert a .\ at the beginning to avoid
        // converting relative paths like a/../c: into c:.
        let colon_in_first_element = buf.iter().take_while(|&&c| !self.is_path_separator(c)).any(|&c| c == b':');
        // If a path begins with \??\, insert a \. at the beginning to avoid converting paths like \a\..\??\c:\x into
        // \??\c:\x (equivalent to c:\x).
        let device_prefix = buf.len() >= 3 && self.is_path_separator(buf[0]) && buf[1] == b'?' && buf[2] == b'?';
        if colon_in_first_element {
            out.prepend(&[b'.', sep]);
        } else if device_prefix {
            out.prepend(&[sep, b'.']);
        }
    }

    // filepathlite Dir
    pub(crate) fn dir(self, path: &str) -> String {
        let vol = self.volume_name(path);
        let bytes = path.as_bytes();
        let mut end = path.len();
        while end > vol.len() && !self.is_path_separator(bytes[end - 1]) {
            end -= 1;
        }
        let dir = self.clean(&path[vol.len()..end]);
        if dir == "." && vol.len() > 2 {
            // must be UNC
            return vol;
        }
        format!("{vol}{dir}")
    }
}
