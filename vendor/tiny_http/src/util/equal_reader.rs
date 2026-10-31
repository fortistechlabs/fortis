use std::io::Read;
use std::io::Result as IoResult;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

/// A `Reader` that reads exactly the number of bytes from a sub-reader.
///
/// If the limit is reached, it returns EOF. If the limit is not reached
/// when the destructor is called, the remaining bytes will be read and
/// thrown away.
pub struct EqualReader<R>
where
    R: Read,
{
    reader: R,
    size: usize,
    last_read_signal: Sender<IoResult<()>>,
    /// FORTIS PATCH: set when this reader is dropped with body bytes still
    /// unread on the socket, so the connection must be closed (see `Drop`).
    abandoned: Option<Arc<AtomicBool>>,
}

impl<R> EqualReader<R>
where
    R: Read,
{
    pub fn new(reader: R, size: usize) -> (EqualReader<R>, Receiver<IoResult<()>>) {
        let (tx, rx) = channel();

        let r = EqualReader {
            reader,
            size,
            last_read_signal: tx,
            abandoned: None,
        };

        (r, rx)
    }

    /// FORTIS PATCH: `flag` is set if the body is left unread on the socket.
    pub fn with_abandoned_flag(mut self, flag: Arc<AtomicBool>) -> EqualReader<R> {
        self.abandoned = Some(flag);
        self
    }
}

impl<R> Read for EqualReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if self.size == 0 {
            return Ok(0);
        }

        let buf = if buf.len() < self.size {
            buf
        } else {
            &mut buf[..self.size]
        };

        match self.reader.read(buf) {
            Ok(len) => {
                self.size -= len;
                Ok(len)
            }
            err @ Err(_) => err,
        }
    }
}

impl<R> Drop for EqualReader<R>
where
    R: Read,
{
    fn drop(&mut self) {
        // FORTIS PATCH. Upstream tiny_http 0.12.0 allocates `vec![0; remaining]`
        // here, so a request declaring a huge Content-Length aborts the whole
        // process on allocation failure. Now: a small unread remainder
        // (`<= MAX_DRAIN`) is drained through a fixed stack buffer so a
        // keep-alive connection stays aligned. A larger one is NOT read; but
        // the bytes are still on the socket and would be parsed as the next
        // request (request smuggling behind a proxy that reuses upstream
        // connections), so the connection is flagged `abandoned` and the
        // server closes it after the response (`ClientConnection::read`).
        const MAX_DRAIN: usize = 64 * 1024;
        let abandon = |flag: &Option<Arc<AtomicBool>>| {
            if let Some(f) = flag {
                f.store(true, Ordering::SeqCst);
            }
        };
        if self.size > MAX_DRAIN {
            abandon(&self.abandoned);
            return;
        }
        let mut remaining_to_read = self.size;
        let mut buf = [0u8; 8192];

        while remaining_to_read > 0 {
            let n = remaining_to_read.min(buf.len());
            match self.reader.read(&mut buf[..n]) {
                Err(e) => {
                    self.last_read_signal.send(Err(e)).ok();
                    abandon(&self.abandoned);
                    break;
                }
                Ok(0) => {
                    self.last_read_signal.send(Ok(())).ok();
                    abandon(&self.abandoned);
                    break;
                }
                Ok(other) => {
                    remaining_to_read -= other;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EqualReader;
    use std::io::Read;

    #[test]
    fn test_limit() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut string = String::new();
            equal_reader.read_to_string(&mut string).unwrap();
            assert_eq!(string, "hello");
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }

    #[test]
    fn test_not_enough() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut vec = [0];
            equal_reader.read_exact(&mut vec).unwrap();
            assert_eq!(vec[0], b'h');
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }

    #[test]
    fn dropping_with_a_huge_unread_remainder_does_not_allocate_it() {
        // Would try to allocate usize::MAX / 2 bytes before the patch.
        let (reader, _rx) = EqualReader::new(std::io::Cursor::new(vec![1u8; 100]), usize::MAX / 2);
        drop(reader);
    }
}
