//! The channel between `iph` and its daemon: a Unix socket on macOS, a named
//! pipe on Windows. Either way one connection carries one request line and
//! one reply line.

use tokio::io::{AsyncRead, AsyncWrite};

pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

pub type Stream = Box<dyn Duplex>;

pub use imp::{Listener, cleanup, connect};

#[cfg(unix)]
mod imp {
    use std::os::unix::fs::PermissionsExt;

    use anyhow::{Context, Result};
    use tokio::net::{UnixListener, UnixStream};

    use super::Stream;
    use crate::paths;

    pub async fn connect(udid: &str) -> std::io::Result<Stream> {
        Ok(Box::new(UnixStream::connect(paths::socket(udid)).await?))
    }

    pub struct Listener(UnixListener);

    impl Listener {
        pub fn bind(udid: &str) -> Result<Self> {
            let sock = paths::socket(udid);
            let _ = std::fs::remove_file(&sock);
            let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self(listener))
        }

        pub async fn accept(&mut self) -> std::io::Result<Stream> {
            let (stream, _) = self.0.accept().await?;
            Ok(Box::new(stream))
        }
    }

    /// Remove what `bind` left behind.
    pub fn cleanup(udid: &str) {
        let _ = std::fs::remove_file(paths::socket(udid));
    }
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use anyhow::{Context, Result};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    use super::Stream;

    /// ERROR_PIPE_BUSY: every instance is taken; the server makes a new one
    /// as soon as it accepts.
    const PIPE_BUSY: i32 = 231;

    fn name(udid: &str) -> String {
        format!(r"\\.\pipe\iph-{udid}")
    }

    pub async fn connect(udid: &str) -> std::io::Result<Stream> {
        for _ in 0..100 {
            match ClientOptions::new().open(name(udid)) {
                Ok(client) => return Ok(Box::new(client)),
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("the daemon's pipe stayed busy"))
    }

    pub struct Listener {
        name: String,
        next: NamedPipeServer,
    }

    impl Listener {
        pub fn bind(udid: &str) -> Result<Self> {
            let name = name(udid);
            // First instance only: a second daemon for the same phone fails here.
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)
                .with_context(|| format!("create {name}"))?;
            Ok(Self { name, next })
        }

        pub async fn accept(&mut self) -> std::io::Result<Stream> {
            self.next.connect().await?;
            let fresh = ServerOptions::new().reject_remote_clients(true).create(&self.name)?;
            Ok(Box::new(std::mem::replace(&mut self.next, fresh)))
        }
    }

    /// A pipe goes away with its process; nothing to remove.
    pub fn cleanup(_udid: &str) {}
}
