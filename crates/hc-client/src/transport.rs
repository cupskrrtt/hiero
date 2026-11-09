//! The byte stream to the core: a Unix socket on Unix, a named pipe on Windows.

use std::io;
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};

pub type Reader = Box<dyn AsyncRead + Send + Unpin>;
pub type Writer = Box<dyn AsyncWrite + Send + Unpin>;

#[cfg(unix)]
pub async fn connect(path: &Path) -> io::Result<(Reader, Writer)> {
    let (r, w) = tokio::net::UnixStream::connect(path).await?.into_split();
    Ok((Box::new(r), Box::new(w)))
}

/// `ERROR_PIPE_BUSY`: every pipe instance is taken for a moment; the core makes a new one soon.
#[cfg(windows)]
const PIPE_BUSY: i32 = 231;

#[cfg(windows)]
pub async fn connect(path: &Path) -> io::Result<(Reader, Writer)> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let mut tries = 0;
    let pipe = loop {
        match ClientOptions::new().open(path) {
            Ok(p) => break p,
            Err(e) if e.raw_os_error() == Some(PIPE_BUSY) && tries < 50 => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    };
    let (r, w) = tokio::io::split(pipe);
    Ok((Box::new(r), Box::new(w)))
}
