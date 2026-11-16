use std::cell::RefCell;

/// Random-access input. Implementations sit over a `File`, a `Blob` or HTTP
/// Range requests; reads are synchronous from the demuxer's point of view
/// (a JS host answers from a worker with `Atomics.wait`, or prefetches).
pub trait ByteSource {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Reads up to `buf.len()` bytes at `offset`. Returns fewer only at end of source.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, String>;
}

pub fn read_exact_at(src: &dyn ByteSource, offset: u64, buf: &mut [u8]) -> Result<(), crate::Error> {
    let end = offset
        .checked_add(buf.len() as u64)
        .ok_or(crate::Error::Source("offset overflow".into()))?;
    if end > src.len() {
        return Err(crate::Error::Source("read past end of source".into()));
    }
    let mut done = 0;
    while done < buf.len() {
        let n = src.read_at(offset + done as u64, &mut buf[done..]).map_err(crate::Error::Source)?;
        if n == 0 {
            return Err(crate::Error::Source("short read".into()));
        }
        done += n;
    }
    Ok(())
}

pub struct SliceSource<'a>(pub &'a [u8]);

impl ByteSource for SliceSource<'_> {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, String> {
        let start = usize::try_from(offset).map_err(|e| e.to_string())?.min(self.0.len());
        let n = buf.len().min(self.0.len() - start);
        buf[..n].copy_from_slice(&self.0[start..start + n]);
        Ok(n)
    }
}

#[cfg(unix)]
pub struct FileSource {
    file: std::fs::File,
    len: u64,
}

#[cfg(unix)]
impl FileSource {
    pub fn open(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}

#[cfg(unix)]
impl ByteSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, String> {
        use std::os::unix::fs::FileExt;
        self.file.read_at(buf, offset).map_err(|e| e.to_string())
    }
}

/// Read-ahead window for sources where every `read_at` is a round trip.
pub struct BufferedSource<S> {
    inner: S,
    window: usize,
    cache: RefCell<(u64, Vec<u8>)>,
}

impl<S: ByteSource> BufferedSource<S> {
    pub fn new(inner: S, window: usize) -> Self {
        Self { inner, window: window.max(4096), cache: RefCell::new((0, Vec::new())) }
    }
}

impl<S: ByteSource> ByteSource for BufferedSource<S> {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, String> {
        if buf.len() >= self.window {
            return self.inner.read_at(offset, buf);
        }
        let mut cache = self.cache.borrow_mut();
        let hit = offset >= cache.0 && offset + buf.len() as u64 <= cache.0 + cache.1.len() as u64;
        if !hit {
            let want = (self.window as u64).min(self.inner.len().saturating_sub(offset)) as usize;
            let mut fresh = vec![0; want];
            let mut got = 0;
            while got < want {
                let n = self.inner.read_at(offset + got as u64, &mut fresh[got..])?;
                if n == 0 {
                    break;
                }
                got += n;
            }
            fresh.truncate(got);
            *cache = (offset, fresh);
        }
        let from = (offset - cache.0) as usize;
        let n = buf.len().min(cache.1.len().saturating_sub(from));
        buf[..n].copy_from_slice(&cache.1[from..from + n]);
        Ok(n)
    }
}
