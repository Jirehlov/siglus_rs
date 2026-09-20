//! Dependency-free subset of `std` for Horizon codec crates.
//!
//! This must not depend on libnx: `byteorder` sits below libnx in the Switch
//! dependency graph and uses this facade to expose its `ReadBytesExt` traits.
#![no_std]

extern crate alloc;

pub use core::mem;
pub use core::slice;

pub mod io {
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ErrorKind {
        UnexpectedEof,
        InvalidData,
        Other,
    }

    #[derive(Debug)]
    pub struct Error {
        kind: ErrorKind,
        message: String,
    }

    impl Error {
        pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
            Self {
                kind,
                message: message.into(),
            }
        }

        pub fn kind(&self) -> ErrorKind {
            self.kind
        }
    }

    impl From<ErrorKind> for Error {
        fn from(kind: ErrorKind) -> Self {
            Self::new(kind, "Switch I/O error")
        }
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.message)
        }
    }

    impl core::error::Error for Error {}

    pub type Result<T, E = Error> = core::result::Result<T, E>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SeekFrom {
        Start(u64),
        End(i64),
        Current(i64),
    }

    pub trait Read {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

        fn read_exact(&mut self, mut buf: &mut [u8]) -> Result<()> {
            while !buf.is_empty() {
                match self.read(buf)? {
                    0 => return Err(Error::new(ErrorKind::UnexpectedEof, "unexpected EOF")),
                    n => buf = &mut buf[n..],
                }
            }
            Ok(())
        }
    }

    pub trait Write {
        fn write(&mut self, buf: &[u8]) -> Result<usize>;
        fn flush(&mut self) -> Result<()>;

        fn write_all(&mut self, mut buf: &[u8]) -> Result<()> {
            while !buf.is_empty() {
                let n = self.write(buf)?;
                if n == 0 {
                    return Err(Error::new(ErrorKind::UnexpectedEof, "short write"));
                }
                buf = &buf[n..];
            }
            Ok(())
        }
    }

    pub trait Seek {
        fn seek(&mut self, pos: SeekFrom) -> Result<u64>;

        fn rewind(&mut self) -> Result<()> {
            self.seek(SeekFrom::Start(0))?;
            Ok(())
        }

        fn stream_position(&mut self) -> Result<u64> {
            self.seek(SeekFrom::Current(0))
        }
    }

    #[derive(Debug, Clone)]
    pub struct Cursor<T> {
        inner: T,
        pos: u64,
    }

    impl<T> Cursor<T> {
        pub fn new(inner: T) -> Self {
            Self { inner, pos: 0 }
        }

        pub fn position(&self) -> u64 {
            self.pos
        }

        pub fn set_position(&mut self, pos: u64) {
            self.pos = pos;
        }

        pub fn get_ref(&self) -> &T {
            &self.inner
        }

        pub fn into_inner(self) -> T {
            self.inner
        }
    }

    impl<T: AsRef<[u8]>> Read for Cursor<T> {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let data = self.inner.as_ref();
            let start = (self.pos as usize).min(data.len());
            let n = buf.len().min(data.len().saturating_sub(start));
            buf[..n].copy_from_slice(&data[start..start + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl<T: AsRef<[u8]>> Seek for Cursor<T> {
        fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
            let len = self.inner.as_ref().len() as i64;
            self.pos = match pos {
                SeekFrom::Start(n) => n,
                SeekFrom::End(d) => (len + d).max(0) as u64,
                SeekFrom::Current(d) => (self.pos as i64 + d).max(0) as u64,
            };
            Ok(self.pos)
        }
    }

    impl Write for Cursor<Vec<u8>> {
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            let pos = self.pos as usize;
            let end = pos.saturating_add(buf.len());
            if self.inner.len() < end {
                self.inner.resize(end, 0);
            }
            self.inner[pos..end].copy_from_slice(buf);
            self.pos = end as u64;
            Ok(buf.len())
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    impl Write for Vec<u8> {
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            self.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    impl Read for &[u8] {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let n = buf.len().min(self.len());
            let (head, rest) = self.split_at(n);
            buf[..n].copy_from_slice(head);
            *self = rest;
            Ok(n)
        }
    }

    impl<R: Read + ?Sized> Read for &mut R {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            (**self).read(buf)
        }
    }

    impl<W: Write + ?Sized> Write for &mut W {
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            (**self).write(buf)
        }

        fn flush(&mut self) -> Result<()> {
            (**self).flush()
        }
    }

    impl<S: Seek + ?Sized> Seek for &mut S {
        fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
            (**self).seek(pos)
        }
    }
}
