//! Shared helpers for logging drivers.

use arrayvec::ArrayVec;

/// Capacity of the per-stream partial-line buffer used by several drivers.
pub const STDIO_BUF_SIZE: usize = 8192;

/// Returns `(line_len, is_partial)` for the next line in `buf`.
///
/// `line_len` includes the trailing newline when `is_partial` is false.
pub fn get_line_len(buf: &[u8]) -> (usize, bool) {
    if let Some(pos) = buf.iter().position(|&c| c == b'\n') {
        (pos + 1, false)
    } else {
        (buf.len(), true)
    }
}

/// Drop trailing newlines. A buffer of only newlines becomes empty.
pub fn strip_trailing_newlines(buf: &[u8]) -> &[u8] {
    match buf.iter().rposition(|&b| b != b'\n') {
        Some(last) => &buf[..=last],
        None => &[],
    }
}

/// Join the buffered bytes with `extra` and clear the buffer.
pub fn take_line(partial: &mut ArrayVec<u8, STDIO_BUF_SIZE>, extra: &[u8]) -> Vec<u8> {
    let mut line = Vec::with_capacity(partial.len() + extra.len());
    line.extend(partial.drain(..));
    line.extend_from_slice(extra);
    line
}

/// Extract the next complete line from `input`, using `partial` for leftovers.
///
/// An empty `input` flushes any buffered bytes. A partial that fills
/// [`STDIO_BUF_SIZE`] is emitted without a newline. Advances `input` past
/// the consumed bytes.
pub fn next_stdio_line(
    partial: &mut ArrayVec<u8, STDIO_BUF_SIZE>,
    input: &mut &[u8],
) -> Option<Vec<u8>> {
    if input.is_empty() {
        if partial.is_empty() {
            return None;
        }
        return Some(take_line(partial, &[]));
    }

    let (len, incomplete) = get_line_len(input);
    if incomplete {
        if let Ok(()) = partial.try_extend_from_slice(&input[..len]) {
            *input = &input[len..];
            if partial.len() < STDIO_BUF_SIZE {
                return None;
            }
            return Some(take_line(partial, &[]));
        }
    }

    let line = take_line(partial, &input[..len]);
    *input = &input[len..];
    Some(line)
}

/// First 12 Unicode scalars of `s`, used as a short container id tag.
pub fn truncate_cuuid(s: &str) -> &str {
    match s.char_indices().nth(12) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_line_len_finds_newline_or_marks_partial() {
        assert_eq!(get_line_len(b"a\nb"), (2, false));
        assert_eq!(get_line_len(b"abc\ndef"), (4, false));
        assert_eq!(get_line_len(b"abc"), (3, true));
        assert_eq!(get_line_len(b""), (0, true));
    }

    #[test]
    fn strip_trailing_newlines_clears_newline_only_buffers() {
        assert_eq!(strip_trailing_newlines(b"hi\n\n"), b"hi");
        assert_eq!(strip_trailing_newlines(b"\n\n"), b"");
        assert_eq!(strip_trailing_newlines(b"hi"), b"hi");
    }

    #[test]
    fn truncate_cuuid_keeps_twelve_chars() {
        assert_eq!(truncate_cuuid("0123456789ab"), "0123456789ab");
        assert_eq!(truncate_cuuid("0123456789abcdef"), "0123456789ab");
        assert_eq!(truncate_cuuid("short"), "short");
    }

    #[test]
    fn next_stdio_line_buffers_partials_and_flushes() {
        let mut partial = ArrayVec::<u8, STDIO_BUF_SIZE>::new();
        let mut input: &[u8] = b"he";
        assert!(next_stdio_line(&mut partial, &mut input).is_none());
        assert_eq!(partial.as_slice(), b"he");
        assert!(input.is_empty());

        input = b"llo\nmore";
        assert_eq!(
            next_stdio_line(&mut partial, &mut input).unwrap(),
            b"hello\n"
        );
        assert_eq!(input, b"more");
        assert!(partial.is_empty());

        assert!(next_stdio_line(&mut partial, &mut input).is_none());
        assert_eq!(partial.as_slice(), b"more");
        input = b"";
        assert_eq!(next_stdio_line(&mut partial, &mut input).unwrap(), b"more");
        assert!(partial.is_empty());
    }
}
