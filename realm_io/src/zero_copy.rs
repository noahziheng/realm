use std::io::{Result, Error, ErrorKind};
use std::pin::Pin;
use std::task::{Poll, Context, ready};
use std::os::unix::io::{RawFd, AsRawFd};

use tokio::io::Interest;
use tokio::io::{AsyncRead, AsyncWrite};

use super::{CopyBuffer, AsyncIOBuf};

pub struct Pipe(RawFd, RawFd);

impl Pipe {
    pub fn new() -> Result<Self> {
        use libc::{c_int, O_NONBLOCK};
        let mut pipe = std::mem::MaybeUninit::<[c_int; 2]>::uninit();
        unsafe {
            if libc::pipe2(pipe.as_mut_ptr() as *mut c_int, O_NONBLOCK) < 0 {
                return Err(Error::last_os_error());
            }

            let [rd, wr] = pipe.assume_init();

            // ignore errno
            // if CUSTOM_PIPE_CAP != DEFAULT_PIPE_CAP {
            //     libc::fcntl(wr, libc::F_SETPIPE_SZ, CUSTOM_PIPE_CAP);
            // }

            Ok(Pipe(rd, wr))
        }
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
            libc::close(self.1);
        }
    }
}

pub trait AsyncRawIO: AsyncRead + AsyncWrite + AsRawFd {
    fn x_poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<()>>;
    fn x_poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Result<()>>;
    fn x_try_io<R>(
        &self,
        interest: Interest,
        f: impl FnOnce() -> Result<R>,
    ) -> Result<R>;
}

impl<S> AsyncIOBuf for CopyBuffer<Pipe, S>
where
    S: AsyncRawIO + Unpin,
{
    type Stream = S;

    fn poll_read_buf(
        &mut self,
        cx: &mut Context<'_>,
        stream: &mut Self::Stream,
    ) -> Poll<Result<usize>> {
        loop {
            ready!(stream.x_poll_read_ready(cx))?;

            let mut is_wouldblock = false;
            let res = stream.x_try_io(Interest::READABLE, || {
                match splice_n(stream.as_raw_fd(), self.buf.1, usize::MAX) {
                    x if x >= 0 => Ok(x as usize),
                    _ => Err(handle_wouldblock(&mut is_wouldblock)),
                }
            });

            if !is_wouldblock {
                return Poll::Ready(res);
            }
        }
    }

    fn poll_write_buf(
        &mut self,
        cx: &mut Context<'_>,
        stream: &mut Self::Stream,
    ) -> Poll<Result<usize>> {
        loop {
            ready!(stream.x_poll_write_ready(cx)?);

            let mut is_wouldblock = false;
            let res = stream.x_try_io(Interest::WRITABLE, || {
                match splice_n(
                    self.buf.0,
                    stream.as_raw_fd(),
                    self.cap - self.pos,
                ) {
                    x if x >= 0 => Ok(x as usize),
                    _ => Err(handle_wouldblock(&mut is_wouldblock)),
                }
            });

            if !is_wouldblock {
                return Poll::Ready(res);
            }
        }
    }

    fn poll_flush_buf(
        &mut self,
        cx: &mut Context<'_>,
        stream: &mut Self::Stream,
    ) -> Poll<Result<()>> {
        Pin::new(stream).poll_flush(cx)
    }
}

#[inline]
fn splice_n(r: RawFd, w: RawFd, n: usize) -> isize {
    use libc::{loff_t, SPLICE_F_MOVE, SPLICE_F_NONBLOCK};
    unsafe {
        libc::splice(
            r,
            std::ptr::null_mut::<loff_t>(),
            w,
            std::ptr::null_mut::<loff_t>(),
            n,
            SPLICE_F_MOVE | SPLICE_F_NONBLOCK,
        )
    }
}

#[inline]
fn handle_wouldblock(is_wouldblock: &mut bool) -> Error {
    use libc::{EWOULDBLOCK, EAGAIN};
    let err = Error::last_os_error();
    match err.raw_os_error() {
        Some(e) if e == EWOULDBLOCK || e == EAGAIN => {
            *is_wouldblock = true;
            ErrorKind::WouldBlock.into()
        }
        _ => err,
    }
}

mod tokio_net {
    use tokio::net::{TcpStream, UnixStream};
    use super::AsyncRawIO;
    use super::*;

    macro_rules! delegate {
        ($stream: ident) => {
            impl AsyncRawIO for $stream {
                #[inline]
                fn x_poll_read_ready(
                    &self,
                    cx: &mut Context<'_>,
                ) -> Poll<Result<()>> {
                    self.poll_read_ready(cx)
                }

                #[inline]
                fn x_poll_write_ready(
                    &self,
                    cx: &mut Context<'_>,
                ) -> Poll<Result<()>> {
                    self.poll_write_ready(cx)
                }

                #[inline]
                fn x_try_io<R>(
                    &self,
                    interest: Interest,
                    f: impl FnOnce() -> Result<R>,
                ) -> Result<R> {
                    self.try_io(interest, f)
                }
            }
        };
    }

    delegate!(TcpStream);
    delegate!(UnixStream);
}