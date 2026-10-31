use std::io::Read;
use std::io::Result as IoResult;
use std::sync::mpsc::channel;
use std::sync::mpsc::{Receiver, Sender};

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
        };

        (r, rx)
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
        // FORTIS PATCH (tiny_http 0.12.0 upstream allocates `vec![0; remaining]`
        // here, so a request declaring a huge Content-Length aborts the whole
        // process on allocation failure): only drain small unread remainders
        // (`<= MAX_DRAIN`) through a fixed stack buffer so normal keep-alive
        // connections stay aligned, and skip draining entirely when the unread
        // remainder exceeds `MAX_DRAIN` so a rejected oversized request cannot
        // tie up a worker thread reading megabytes of discarded body.
        const MAX_DRAIN: usize = 64 * 1024;
        if self.size > MAX_DRAIN {
            return;
        }
        let mut remaining_to_read = self.size;
        let mut buf = [0u8; 8192];

        while remaining_to_read > 0 {
            let n = remaining_to_read.min(buf.len());
            match self.reader.read(&mut buf[..n]) {
                Err(e) => {
                    self.last_read_signal.send(Err(e)).ok();
                    break;
                }
                Ok(0) => {
                    self.last_read_signal.send(Ok(())).ok();
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
