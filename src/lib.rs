use std::io::{self, Result};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

use libc;

use mio::unix::EventedFd;
use mio::{self, Evented, PollOpt, Ready, Token};

use futures::{Async, Poll};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_reactor::PollEvented;

pub enum FifoMode {
    Read,
    Write,
}

struct Inner(RawFd);

impl Inner {
    fn create<P: AsRef<Path>>(path: P, perm: u32) -> Result<()> {
        let path = path.as_ref().as_os_str();
        let path = path.as_bytes();
        let mut buf = [0; libc::PATH_MAX as usize];
        buf[..path.len()].copy_from_slice(path);
        let rv = unsafe { libc::mkfifo(buf.as_ptr() as *const i8, perm) };
        if rv < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn open<P: AsRef<Path>>(path: P, mode: FifoMode) -> Result<Self> {
        let path = path.as_ref().as_os_str();
        let path = path.as_bytes();
        let mut buf = [0; libc::PATH_MAX as usize];
        buf[..path.len()].copy_from_slice(path);

        let flag = match mode {
            FifoMode::Read => libc::O_RDONLY,
            FifoMode::Write => libc::O_WRONLY,
        };
        let flag = flag | libc::O_NONBLOCK;
        let rv = unsafe { libc::open(buf.as_ptr() as *const i8, flag) };
        if rv < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Inner(rv))
    }
}

impl io::Read for Inner {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let rv =
            unsafe { libc::read(self.0, buf.as_mut_ptr() as *mut std::ffi::c_void, buf.len()) };
        if rv < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(rv as usize)
    }
}

impl io::Write for Inner {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let rv = unsafe { libc::write(self.0, buf.as_ptr() as *const std::ffi::c_void, buf.len()) };
        if rv < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(rv as usize)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl Evented for Inner {
    fn register(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> Result<()> {
        poll.register(&EventedFd(&self.0), token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> Result<()> {
        poll.reregister(&EventedFd(&self.0), token, interest, opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> Result<()> {
        poll.deregister(&EventedFd(&self.0))
    }
}

pub struct FifoListener(PathBuf);

impl FifoListener {
    pub fn bind<P: AsRef<Path>>(path: P, perm: u32) -> Result<Self> {
        let _ = Inner::create(path.as_ref(), perm)?;
        Ok(FifoListener(path.as_ref().to_path_buf()))
    }

    pub fn sink(self) -> Result<FifoSink> {
        let inner = Inner::open(&self.0, FifoMode::Write)?;
        Ok(FifoSink(PollEvented::new(inner)))
    }

    pub fn stream(self) -> Result<FifoStream> {
        let inner = Inner::open(&self.0, FifoMode::Read)?;
        Ok(FifoStream(PollEvented::new(inner)))
    }
}

pub struct FifoSink(PollEvented<Inner>);

impl FifoSink {
    pub fn connect<P: AsRef<Path>>(path: P) -> Result<Self> {
        let inner = Inner::open(path.as_ref(), FifoMode::Write)?;
        Ok(FifoSink(PollEvented::new(inner)))
    }

    pub fn poll_write_ready(&self) -> Poll<Ready, io::Error> {
        self.0.poll_write_ready()
    }

    pub fn clear_write_ready(&self) -> Result<()> {
        self.0.clear_write_ready()
    }
}

pub struct FifoStream(PollEvented<Inner>);

impl FifoStream {
    pub fn connect<P: AsRef<Path>>(path: P) -> Result<Self> {
        let inner = Inner::open(path.as_ref(), FifoMode::Read)?;
        Ok(FifoStream(PollEvented::new(inner)))
    }

    pub fn poll_read_ready(&self, mask: Ready) -> Poll<Ready, io::Error> {
        self.0.poll_read_ready(mask)
    }

    pub fn clear_read_ready(&self, mask: Ready) -> Result<()> {
        self.0.clear_read_ready(mask)
    }
}

impl io::Read for FifoStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.0.read(buf)
    }
}

impl AsyncRead for FifoStream {
    fn poll_read(&mut self, buf: &mut [u8]) -> Poll<usize, io::Error> {
        self.0.poll_read(buf)
    }
}

impl io::Write for FifoSink {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> Result<()> {
        self.0.flush()
    }
}

impl AsyncWrite for FifoSink {
    fn poll_write(&mut self, buf: &[u8]) -> Poll<usize, io::Error> {
        self.0.poll_write(buf)
    }

    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(Async::Ready(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::{BufMut, ByteOrder, BytesMut, LittleEndian};
    use std::time::Duration;
    use tokio::codec::{Decoder, Encoder, FramedRead, FramedWrite};
    use tokio::prelude::*;
    use tokio::timer::Interval;

    struct C;

    impl Encoder for C {
        type Item = u64;
        type Error = std::io::Error;

        fn encode(&mut self, item: Self::Item, dst: &mut BytesMut) -> Result<()> {
            dst.put_u64::<LittleEndian>(item);
            Ok(())
        }
    }

    impl Decoder for C {
        type Item = u64;
        type Error = std::io::Error;

        fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
            if src.len() < 8 {
                return Ok(None);
            }
            let rv = LittleEndian::read_u64(&src);
            src.advance(8);
            Ok(Some(rv))
        }
    }

    #[test]
    fn created() {
        let listener = FifoListener::bind("/tmp/kek.fifo", 0o666).unwrap();
        let listener = FramedRead::new(listener.stream().unwrap(), C);

        let stream = FifoSink::connect("/tmp/kek.fifo").unwrap();
        let stream = FramedWrite::new(stream, C);

        tokio::run(future::lazy(|| {
            tokio::spawn(future::lazy(|| {
                listener
                    .for_each(|msg| {
                        println!("received {}", msg);
                        Ok(())
                    })
                    .map_err(|err| panic!(err))
            }));
            Interval::new_interval(Duration::from_secs(1))
                .map(|_| 1u64)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "timer!"))
                .forward(stream)
                .map_err(|err| panic!(err))
                .map(|_| ())
        }))
    }
}
