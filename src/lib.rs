//! Async TLS streams backed by OpenSSL.
//!
//! This crate provides a wrapper around the [`openssl`] crate's [`SslStream`](ssl::SslStream) type
//! that works with with [`tokio`]'s [`AsyncRead`] and [`AsyncWrite`] traits rather than std's
//! blocking [`Read`] and [`Write`] traits.
#![warn(missing_docs)]

use futures_util::future;
use openssl::error::ErrorStack;
use openssl::ssl::{self, ErrorCode, ShutdownResult, Ssl, SslRef};
use std::fmt;
use std::io::{self, Read, Write};
use std::pin::Pin;
use std::slice;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(test)]
mod test;

struct StreamWrapper<S> {
    stream: S,
    context: usize,
}

impl<S> fmt::Debug for StreamWrapper<S>
where
    S: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.stream, fmt)
    }
}

impl<S> StreamWrapper<S> {
    /// # Safety
    ///
    /// Must be called with `context` set to a valid pointer to a live `Context` object, and the
    /// wrapper must be pinned in memory.
    unsafe fn parts(&mut self) -> (Pin<&mut S>, &mut Context<'_>) {
        debug_assert_ne!(self.context, 0);
        let stream = Pin::new_unchecked(&mut self.stream);
        let context = &mut *(self.context as *mut _);
        (stream, context)
    }
}

impl<S> Read for StreamWrapper<S>
where
    S: AsyncRead,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (stream, cx) = unsafe { self.parts() };
        let mut buf = ReadBuf::new(buf);
        match stream.poll_read(cx, &mut buf)? {
            Poll::Ready(()) => Ok(buf.filled().len()),
            Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        }
    }
}

impl<S> Write for StreamWrapper<S>
where
    S: AsyncWrite,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let (stream, cx) = unsafe { self.parts() };
        match stream.poll_write(cx, buf) {
            Poll::Ready(r) => r,
            Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let (stream, cx) = unsafe { self.parts() };
        match stream.poll_flush(cx) {
            Poll::Ready(r) => r,
            Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        }
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        let (stream, cx) = unsafe { self.parts() };
        match stream.poll_write_vectored(cx, bufs) {
            Poll::Ready(r) => r,
            Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        }
    }
}

fn cvt<T>(r: io::Result<T>) -> Poll<io::Result<T>> {
    match r {
        Ok(v) => Poll::Ready(Ok(v)),
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Poll::Pending,
        Err(e) => Poll::Ready(Err(e)),
    }
}

fn cvt_ossl<T>(r: Result<T, ssl::Error>) -> Poll<Result<T, ssl::Error>> {
    match r {
        Ok(v) => Poll::Ready(Ok(v)),
        Err(e) => match e.code() {
            ErrorCode::WANT_READ | ErrorCode::WANT_WRITE => Poll::Pending,
            _ => Poll::Ready(Err(e)),
        },
    }
}

/// An asynchronous version of [`openssl::ssl::SslStream`].
#[derive(Debug)]
pub struct SslStream<S>(ssl::SslStream<StreamWrapper<S>>);

impl<S> SslStream<S>
where
    S: AsyncRead + AsyncWrite,
{
    /// Like [`SslStream::new`](ssl::SslStream::new).
    pub fn new(ssl: Ssl, stream: S) -> Result<Self, ErrorStack> {
        ssl::SslStream::new(ssl, StreamWrapper { stream, context: 0 }).map(SslStream)
    }

    /// Like [`SslStream::connect`](ssl::SslStream::connect).
    pub fn poll_connect(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), ssl::Error>> {
        self.with_context(cx, |s| cvt_ossl(s.connect()))
    }

    /// A convenience method wrapping [`poll_connect`](Self::poll_connect).
    pub async fn connect(mut self: Pin<&mut Self>) -> Result<(), ssl::Error> {
        future::poll_fn(|cx| self.as_mut().poll_connect(cx)).await
    }

    /// Like [`SslStream::accept`](ssl::SslStream::accept).
    pub fn poll_accept(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), ssl::Error>> {
        self.with_context(cx, |s| cvt_ossl(s.accept()))
    }

    /// A convenience method wrapping [`poll_accept`](Self::poll_accept).
    pub async fn accept(mut self: Pin<&mut Self>) -> Result<(), ssl::Error> {
        future::poll_fn(|cx| self.as_mut().poll_accept(cx)).await
    }

    /// Like [`SslStream::do_handshake`](ssl::SslStream::do_handshake).
    pub fn poll_do_handshake(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), ssl::Error>> {
        self.with_context(cx, |s| cvt_ossl(s.do_handshake()))
    }

    /// A convenience method wrapping [`poll_do_handshake`](Self::poll_do_handshake).
    pub async fn do_handshake(mut self: Pin<&mut Self>) -> Result<(), ssl::Error> {
        future::poll_fn(|cx| self.as_mut().poll_do_handshake(cx)).await
    }

    /// Like [`SslStream::read_early_data`](ssl::SslStream::read_early_data).
    #[cfg(ossl111)]
    pub fn poll_read_early_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, ssl::Error>> {
        self.with_context(cx, |s| cvt_ossl(s.read_early_data(buf)))
    }

    /// A convenience method wrapping [`poll_read_early_data`](Self::poll_read_early_data).
    #[cfg(ossl111)]
    pub async fn read_early_data(
        mut self: Pin<&mut Self>,
        buf: &mut [u8],
    ) -> Result<usize, ssl::Error> {
        future::poll_fn(|cx| self.as_mut().poll_read_early_data(cx, buf)).await
    }

    /// Like [`SslStream::write_early_data`](ssl::SslStream::write_early_data).
    #[cfg(ossl111)]
    pub fn poll_write_early_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, ssl::Error>> {
        self.with_context(cx, |s| cvt_ossl(s.write_early_data(buf)))
    }

    /// A convenience method wrapping [`poll_write_early_data`](Self::poll_write_early_data).
    #[cfg(ossl111)]
    pub async fn write_early_data(
        mut self: Pin<&mut Self>,
        buf: &[u8],
    ) -> Result<usize, ssl::Error> {
        future::poll_fn(|cx| self.as_mut().poll_write_early_data(cx, buf)).await
    }

    /// Like [`SslStream::ssl_read`](ssl::SslStream::ssl_read).
    pub fn poll_ssl_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, ssl::Error>> {
        self.with_context(cx, |s| cvt_ossl(s.ssl_read(buf)))
    }

    /// A convenience method wrapping [`poll_ssl_read`](Self::poll_ssl_read).
    pub async fn ssl_read(mut self: Pin<&mut Self>, buf: &mut [u8]) -> Result<usize, ssl::Error> {
        future::poll_fn(|cx| self.as_mut().poll_ssl_read(cx, buf)).await
    }
}

impl<S> SslStream<S> {
    /// Returns a shared reference to the `Ssl` object associated with this stream.
    pub fn ssl(&self) -> &SslRef {
        self.0.ssl()
    }

    /// Returns a shared reference to the underlying stream.
    pub fn get_ref(&self) -> &S {
        &self.0.get_ref().stream
    }

    /// Returns a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.0.get_mut().stream
    }

    /// Returns a pinned mutable reference to the underlying stream.
    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut S> {
        unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().0.get_mut().stream) }
    }

    fn with_context<F, R>(self: Pin<&mut Self>, ctx: &mut Context<'_>, f: F) -> R
    where
        F: FnOnce(&mut ssl::SslStream<StreamWrapper<S>>) -> R,
    {
        let this = unsafe { self.get_unchecked_mut() };
        this.0.get_mut().context = ctx as *mut _ as usize;
        let r = f(&mut this.0);
        this.0.get_mut().context = 0;
        r
    }
}

impl<S> AsyncRead for SslStream<S>
where
    S: AsyncRead + AsyncWrite,
{
    fn poll_read(
        self: Pin<&mut Self>,
        ctx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.with_context(ctx, |s| {
            // This isn't really "proper", but rust-openssl doesn't currently expose a suitable interface even though
            // OpenSSL itself doesn't require the buffer to be initialized. So this is good enough for now.
            let slice = unsafe {
                let buf = buf.unfilled_mut();
                slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), buf.len())
            };
            match cvt(s.read(slice))? {
                Poll::Ready(nread) => {
                    unsafe {
                        buf.assume_init(nread);
                    }
                    buf.advance(nread);
                    Poll::Ready(Ok(()))
                }
                Poll::Pending => Poll::Pending,
            }
        })
    }
}

impl<S> AsyncWrite for SslStream<S>
where
    S: AsyncRead + AsyncWrite,
{
    fn poll_write(self: Pin<&mut Self>, ctx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.with_context(ctx, |s| cvt(s.write(buf)))
    }

    fn poll_flush(self: Pin<&mut Self>, ctx: &mut Context) -> Poll<io::Result<()>> {
        self.with_context(ctx, |s| cvt(s.flush()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<io::Result<()>> {
        let first_shut_ret = match self.as_mut().with_context(ctx, |s| s.shutdown()) {
            Ok(ShutdownResult::Sent) => {
                // close notify sent but not received from peer
                // another try to wait for peer's close notify
                // We tried call SSL_shutdown() twice previously, we use the recommended SSL_read() now
                // The OpenSSL manpage suggests SSL_read()
                // https://github.com/openssl/openssl/blob/OpenSSL_1_1_1-stable/doc/man3/SSL_shutdown.pod

                _ShouldKeepPollSslRead::ShouldStartPollSslRead
            }
            Ok(ShutdownResult::Received) => {
                // close notify sent and received from peer, finished
                return Poll::Ready(Ok(()));
            }
            Err(ref e) if e.code() == ErrorCode::ZERO_RETURN => {
                // no more read from peer
                return Poll::Ready(Ok(()));
            }
            Err(ref e) if e.code() == ErrorCode::WANT_READ || e.code() == ErrorCode::WANT_WRITE => {
                return Poll::Pending;
            }
            Err(ref e) if e.code() == ErrorCode::SYSCALL && e.io_error().is_none() => {
                // other side closed underlying socket without sending the close notify
                // we assume it is okay
                return Poll::Ready(Ok(()));
            }
            Err(e) => {
                return Poll::Ready(Err(e
                    .into_io_error()
                    .unwrap_or_else(|e| io::Error::new(io::ErrorKind::Other, e))));
            }
        };

        if let _ShouldKeepPollSslRead::ShouldStartPollSslRead = first_shut_ret {
            let mut buf = [0u8; 1024];
            loop {
                match cvt_shutdown_ssl_read_ossl(self.as_mut().poll_ssl_read(ctx, &mut buf)) {
                    Poll::Ready(s) => match s {
                        Ok(keep) => match keep {
                            _ShouldKeepPollSslRead::ShouldStartPollSslRead => {
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::Other,
                                    format!("poll_shutdown: should not go _ShouldKeepPollSslRead::ShouldStartPollSslRead"),
                                )))
                            }
                            _ShouldKeepPollSslRead::KeepPollSslRead => {}
                            _ShouldKeepPollSslRead::Finished => return Poll::Ready(Ok(())),
                        },
                        Err(e) => {
                            return Poll::Ready(Err(e
                                .into_io_error()
                                .unwrap_or_else(|e| io::Error::new(io::ErrorKind::Other, e))));
                        }
                    },
                    Poll::Pending => return Poll::Pending,
                }
            }
        } else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                format!("poll_shutdown: should already returned"),
            )));
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        self.with_context(cx, |s| cvt(s.write_vectored(bufs)))
    }
}

/// A private enum that should not be used
enum _ShouldKeepPollSslRead {
    ShouldStartPollSslRead,
    KeepPollSslRead,
    Finished,
}

/// Convert SSL_read() result to Poll<T> enum when we are doing SSL_shutdown()
fn cvt_shutdown_ssl_read_ossl(
    r: Poll<Result<usize, ssl::Error>>,
) -> Poll<Result<_ShouldKeepPollSslRead, ssl::Error>> {
    match r {
        Poll::Ready(r) => match r {
            Ok(_) => {
                // We read some data, we should keep reading until we got ZERO_RETURN
                Poll::Ready(Ok(_ShouldKeepPollSslRead::KeepPollSslRead))
            }
            Err(e) => match e.code() {
                ErrorCode::WANT_READ | ErrorCode::WANT_WRITE => Poll::Pending,
                ErrorCode::ZERO_RETURN => Poll::Ready(Ok(_ShouldKeepPollSslRead::Finished)),
                // other side closed underlying socket without sending the close notify
                // we assume it is okay
                ErrorCode::SYSCALL if e.io_error().is_none() => {
                    Poll::Ready(Ok(_ShouldKeepPollSslRead::Finished))
                }
                _ => Poll::Ready(Err(e)),
            },
        },
        Poll::Pending => Poll::Pending,
    }
}
