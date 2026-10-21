//! A writer that puts a header line at the left margin and indents everything
//! written through it by two spaces. Nesting adds a level per wrapper.

use std::io::{self, Write};

pub struct Section<W: Write> {
    inner: W,
    header: String,
    started: bool,
    at_line_start: bool,
}

impl<W: Write> Section<W> {
    /// The header is written lazily, on the first byte (or `open`).
    pub fn new(inner: W, header: impl Into<String>) -> Self {
        Self {
            inner,
            header: header.into(),
            started: false,
            at_line_start: true,
        }
    }

    /// Writes the header now if it has not been written yet.
    pub fn open(&mut self) -> io::Result<()> {
        if !self.started {
            self.started = true;
            writeln!(self.inner, "{}", self.header)?;
        }
        Ok(())
    }

    pub fn started(&self) -> bool {
        self.started
    }

    /// The wrapped writer, for output that belongs outside the section. Write
    /// whole lines only, so the section's next line still starts on a fresh one.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for Section<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.open()?;
        let mut rest = buf;
        while !rest.is_empty() {
            if self.at_line_start && rest[0] != b'\n' {
                self.inner.write_all(b"  ")?;
            }
            let end = rest
                .iter()
                .position(|&b| b == b'\n')
                .map_or(rest.len(), |i| i + 1);
            self.inner.write_all(&rest[..end])?;
            self.at_line_start = rest[end - 1] == b'\n';
            rest = &rest[end..];
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(out: Vec<u8>) -> String {
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn lazy_header_only_on_first_write() {
        let s = Section::new(Vec::new(), "H");
        assert!(!s.started());
        assert_eq!(text(s.into_inner()), "");
        let mut s = Section::new(Vec::new(), "H");
        write!(s, "a\nb\n").unwrap();
        assert!(s.started());
        assert_eq!(text(s.into_inner()), "H\n  a\n  b\n");
    }

    #[test]
    fn open_writes_header_without_body() {
        let mut s = Section::new(Vec::new(), "H");
        s.open().unwrap();
        s.open().unwrap();
        assert_eq!(text(s.into_inner()), "H\n");
    }

    #[test]
    fn partial_lines_indent_once() {
        let mut s = Section::new(Vec::new(), "H");
        write!(s, "ab").unwrap();
        writeln!(s, "c").unwrap();
        assert_eq!(text(s.into_inner()), "H\n  abc\n");
    }

    #[test]
    fn nested_sections_indent_twice() {
        let mut outer = Section::new(Vec::new(), "O");
        {
            let mut inner = Section::new(&mut outer, "I");
            writeln!(inner, "x").unwrap();
        }
        assert_eq!(text(outer.into_inner()), "O\n  I\n    x\n");
    }

    #[test]
    fn blank_line_not_indented() {
        let mut s = Section::new(Vec::new(), "H");
        write!(s, "a\n\nb\n").unwrap();
        assert_eq!(text(s.into_inner()), "H\n  a\n\n  b\n");
    }
}
