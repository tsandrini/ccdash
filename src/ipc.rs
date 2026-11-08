//! The collector's local sockets: Unix sockets; on Windows named pipes
//! named after the socket path, open to this user only.
//!
//! - ingest: one message per hook, never waited on (a datagram; on Windows
//!   one pipe connection per message, written then closed)
//! - api: newline-delimited JSON requests and replies

pub use imp::*;

#[cfg(unix)]
mod imp {
    use std::{
        io::{self, Read, Write},
        os::unix::net::{UnixDatagram, UnixStream},
        path::Path,
        time::Duration,
    };

    /// A blocking connection to the API socket (hooks, the CLI).
    pub struct Stream(UnixStream);

    impl Stream {
        pub fn connect(path: &Path) -> io::Result<Self> {
            UnixStream::connect(path).map(Self)
        }

        /// `connect`: a Unix socket never makes it wait.
        pub fn connect_within(path: &Path, _wait: Duration) -> io::Result<Self> {
            Self::connect(path)
        }

        pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.0.set_read_timeout(timeout)
        }

        pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.0.set_write_timeout(timeout)
        }
    }

    impl Read for &Stream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            (&self.0).read(buf)
        }
    }

    impl Write for &Stream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            (&self.0).write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            (&self.0).flush()
        }
    }

    impl Read for Stream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            (&*self).read(buf)
        }
    }

    impl Write for Stream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            (&*self).write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            (&*self).flush()
        }
    }

    /// Whether a collector serves the API socket at `path`.
    pub fn serving(path: &Path) -> bool {
        Stream::connect(path).is_ok()
    }

    /// Where a client of `path` connects, for the log.
    pub fn address(path: &Path) -> String {
        path.display().to_string()
    }

    /// Sends one ingest message without waiting; lost when no collector
    /// listens or its queue is full.
    pub fn send(path: &Path, bytes: &[u8]) {
        if let Ok(socket) = UnixDatagram::unbound() {
            let _ = socket.set_nonblocking(true);
            let _ = socket.send_to(bytes, path);
        }
    }

    /// A connection the API listener accepted.
    pub type Accepted = tokio::net::UnixStream;
    /// An async connection to the API socket (`ccdash channel`).
    pub type Connected = tokio::net::UnixStream;
    pub type ReadHalf = tokio::net::unix::OwnedReadHalf;
    pub type WriteHalf = tokio::net::unix::OwnedWriteHalf;

    pub fn split(stream: Accepted) -> (ReadHalf, WriteHalf) {
        stream.into_split()
    }

    pub async fn connect(path: &Path) -> io::Result<Connected> {
        tokio::net::UnixStream::connect(path).await
    }

    /// The API socket's listener.
    pub struct Listener(tokio::net::UnixListener);

    impl Listener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            tokio::net::UnixListener::bind(path).map(Self)
        }

        pub async fn accept(&mut self) -> io::Result<Accepted> {
            self.0.accept().await.map(|(stream, _)| stream)
        }
    }

    /// The ingest socket's receiving end.
    pub struct Ingest(tokio::net::UnixDatagram);

    impl Ingest {
        pub fn bind(path: &Path) -> io::Result<Self> {
            tokio::net::UnixDatagram::bind(path).map(Self)
        }

        /// The next message; a longer one is cut at `buf`'s length.
        pub async fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.recv(buf).await
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::{
        cell::Cell,
        fs::{File, OpenOptions},
        io::{self, Read, Write},
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        path::Path,
        time::{Duration, Instant},
    };

    use tokio::{
        io::AsyncReadExt,
        net::windows::named_pipe::{
            ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
        },
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_BUSY},
        Storage::FileSystem::{FILE_FLAG_OVERLAPPED, SECURITY_IDENTIFICATION},
    };

    use crate::win;

    /// How long a client waits for a free pipe instance.
    const BUSY_WAIT: Duration = Duration::from_secs(2);
    /// How long an ingest message may take to arrive whole.
    const INGEST_READ: Duration = Duration::from_secs(1);
    /// How long `send` may wait for a pipe instance, and then to write.
    const SEND_WAIT: Duration = Duration::from_millis(500);
    /// Messages accepted and not yet taken by the collector.
    const INGEST_QUEUE: usize = 1024;

    /// The pipe for a socket path: pipe names are machine-wide, so the
    /// path (under the user's profile) is hashed into it, as Windows
    /// compares paths (any case, either separator): `c:\ccdash` in one
    /// environment and `C:\ccdash` in another name one pipe.
    pub fn pipe_name(path: &Path) -> String {
        let key = crate::workspace::path_key(&path.to_string_lossy()).into_owned();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in key.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        let stem = path
            .file_stem()
            .map_or_else(|| "socket".into(), |s| s.to_string_lossy().into_owned());
        format!(r"\\.\pipe\ccdash-{stem}-{hash:016x}")
    }

    fn busy(e: &io::Error) -> bool {
        e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
    }

    fn broken(e: &io::Error) -> bool {
        e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32)
    }

    /// Refuses a pipe another user serves: the names are guessable, and
    /// whoever creates a pipe first serves it.
    fn ours<T: AsRawHandle>(pipe: T) -> io::Result<T> {
        if win::owned_by_me(&pipe)? {
            Ok(pipe)
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "another user serves the pipe",
            ))
        }
    }

    /// A blocking connection to the API pipe (hooks, the CLI), with
    /// timeouts through overlapped I/O.
    pub struct Stream {
        file: File,
        event: win::Event,
        read_timeout: Cell<Option<Duration>>,
        write_timeout: Cell<Option<Duration>>,
    }

    impl Stream {
        pub fn connect(path: &Path) -> io::Result<Self> {
            Self::connect_within(path, BUSY_WAIT)
        }

        /// Gives up when no pipe instance is free within `wait`.
        pub fn connect_within(path: &Path, wait: Duration) -> io::Result<Self> {
            let name = pipe_name(path);
            let open = || {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(FILE_FLAG_OVERLAPPED)
                    .security_qos_flags(SECURITY_IDENTIFICATION)
                    .open(&name)
            };
            // NOTE: every waiter wakes when an instance frees up, and only one gets it
            let deadline = Instant::now() + wait;
            let file = loop {
                match open() {
                    Err(e) if busy(&e) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return Err(e);
                        }
                        win::wait_named_pipe(&name, left);
                    }
                    other => break other?,
                }
            };
            Ok(Self {
                file: ours(file)?,
                event: win::Event::new()?,
                read_timeout: Cell::new(None),
                write_timeout: Cell::new(None),
            })
        }

        pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.read_timeout.set(timeout);
            Ok(())
        }

        pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.write_timeout.set(timeout);
            Ok(())
        }
    }

    impl Read for &Stream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match win::read_overlapped(&self.file, &self.event, buf, self.read_timeout.get()) {
                Err(e) if broken(&e) => Ok(0),
                other => other,
            }
        }
    }

    impl Write for &Stream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            win::write_overlapped(&self.file, &self.event, buf, self.write_timeout.get())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for Stream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            (&*self).read(buf)
        }
    }

    impl Write for Stream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            (&*self).write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Whether something serves the API pipe for `path`: only a pipe that
    /// does not exist says no, a busy one or another user's says yes.
    pub fn serving(path: &Path) -> bool {
        !matches!(Stream::connect(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
    }

    /// Where a client of `path` connects, for the log.
    pub fn address(path: &Path) -> String {
        pipe_name(path)
    }

    /// Sends one ingest message; lost when no collector listens or it does
    /// not take it within `SEND_WAIT`.
    pub fn send(path: &Path, bytes: &[u8]) {
        if let Ok(stream) = Stream::connect_within(path, SEND_WAIT) {
            let _ = stream.set_write_timeout(Some(SEND_WAIT));
            let _ = (&stream).write_all(bytes);
        }
    }

    /// A connection the API listener accepted.
    pub type Accepted = NamedPipeServer;
    /// An async connection to the API pipe (`ccdash channel`).
    pub type Connected = NamedPipeClient;
    pub type ReadHalf = tokio::io::ReadHalf<NamedPipeServer>;
    pub type WriteHalf = tokio::io::WriteHalf<NamedPipeServer>;

    pub fn split(stream: Accepted) -> (ReadHalf, WriteHalf) {
        tokio::io::split(stream)
    }

    pub async fn connect(path: &Path) -> io::Result<Connected> {
        let name = pipe_name(path);
        let deadline = tokio::time::Instant::now() + BUSY_WAIT;
        loop {
            match ClientOptions::new().open(&name) {
                Err(e) if busy(&e) && tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                other => return other.and_then(ours),
            }
        }
    }

    /// The API pipe's listener: one instance always waits for the next
    /// client.
    pub struct Listener {
        name: String,
        security: win::OwnerOnly,
        /// None when making it failed: the next `accept` tries again.
        next: Option<NamedPipeServer>,
    }

    impl Listener {
        /// Fails when another process serves the pipe.
        pub fn bind(path: &Path) -> io::Result<Self> {
            let name = pipe_name(path);
            let security = win::OwnerOnly::new()?;
            let next = Some(instance(&name, &security, true)?);
            Ok(Self {
                name,
                security,
                next,
            })
        }

        /// The next client. An error leaves no connected pipe behind: the
        /// caller may call again, after a pause.
        pub async fn accept(&mut self) -> io::Result<Accepted> {
            let pipe = match self.next.take() {
                Some(pipe) => pipe,
                None => instance(&self.name, &self.security, false)?,
            };
            pipe.connect().await?;
            // NOTE: made at once, so a client finds a free instance while this one is served
            self.next = instance(&self.name, &self.security, false).ok();
            Ok(pipe)
        }
    }

    fn instance(name: &str, security: &win::OwnerOnly, first: bool) -> io::Result<NamedPipeServer> {
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        // SAFETY: the attributes point at a security descriptor `security` keeps alive
        unsafe { options.create_with_security_attributes_raw(name, security.as_ptr()) }
    }

    /// A message being read, in the order its connection came.
    type Pending = tokio::sync::oneshot::Receiver<io::Result<Vec<u8>>>;

    /// The ingest pipe's receiving end: a task accepts connections as they
    /// come and reads each on its own, so a hook waits on neither the
    /// collector nor another hook; messages come out in the order they
    /// connected.
    pub struct Ingest(tokio::sync::mpsc::Receiver<Pending>);

    impl Ingest {
        /// Needs a Tokio runtime: the accepting task starts here.
        pub fn bind(path: &Path) -> io::Result<Self> {
            let mut listener = Listener::bind(path)?;
            let (tx, rx) = tokio::sync::mpsc::channel(INGEST_QUEUE);
            tokio::spawn(async move {
                loop {
                    let pipe = match listener.accept().await {
                        Ok(pipe) => pipe,
                        Err(e) => {
                            tracing::warn!("ingest: accepting: {e}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    let (done, pending) = tokio::sync::oneshot::channel();
                    // NOTE: a full queue stops accepting; waiting hooks give up after SEND_WAIT
                    if tx.send(pending).await.is_err() {
                        return;
                    }
                    tokio::spawn(async move {
                        let _ = done.send(read_message(pipe).await);
                    });
                }
            });
            Ok(Self(rx))
        }

        /// The next message; a longer one is cut at `buf`'s length.
        pub async fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let pending = self.0.recv().await.ok_or(io::ErrorKind::BrokenPipe)?;
            let message = pending.await.map_err(|_| io::ErrorKind::BrokenPipe)??;
            let len = message.len().min(buf.len());
            buf[..len].copy_from_slice(&message[..len]);
            Ok(len)
        }
    }

    /// One connection's message, until the sender closes it.
    async fn read_message(pipe: NamedPipeServer) -> io::Result<Vec<u8>> {
        let read = async {
            // NOTE: grown as it comes: the queue may hold many short messages
            let mut message = Vec::new();
            let limit = (crate::wire::MAX_DATAGRAM + 1024) as u64;
            match pipe.take(limit).read_to_end(&mut message).await {
                Ok(_) => {}
                // NOTE: what came before the sender closed is kept
                Err(e) if broken(&e) => {}
                Err(e) => return Err(e),
            }
            message.shrink_to_fit();
            Ok(message)
        };
        tokio::time::timeout(INGEST_READ, read)
            .await
            .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
    }

    #[cfg(test)]
    mod tests {
        use std::io::{BufRead, BufReader};

        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        use super::*;

        fn socket(name: &str) -> std::path::PathBuf {
            std::env::temp_dir().join(format!("ccdash-ipc-{}-{name}.sock", std::process::id()))
        }

        #[test]
        fn pipe_names_follow_the_path() {
            let a = pipe_name(Path::new(r"C:\Users\a\AppData\Local\ccdash\run\api.sock"));
            let b = pipe_name(Path::new(r"C:\Users\b\AppData\Local\ccdash\run\api.sock"));
            assert!(a.starts_with(r"\\.\pipe\ccdash-api-"), "{a}");
            assert_ne!(a, b);
            assert_eq!(
                pipe_name(Path::new(r"c:\ccdash\api.sock")),
                pipe_name(Path::new("C:/CCDASH/api.sock"))
            );
        }

        #[tokio::test]
        async fn requests_and_replies_round_trip() {
            let path = socket("api");
            let mut listener = Listener::bind(&path).unwrap();
            assert!(
                Listener::bind(&path).is_err(),
                "a second server must be refused"
            );
            let server = tokio::spawn(async move {
                let (read, mut write) = split(listener.accept().await.unwrap());
                let mut lines = tokio::io::BufReader::new(read).lines();
                let line = lines.next_line().await.unwrap().unwrap();
                write
                    .write_all(format!("{line}!\n").as_bytes())
                    .await
                    .unwrap();
            });
            let reply = tokio::task::spawn_blocking(move || {
                let mut stream = Stream::connect(&path).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream.write_all(b"ping\n").unwrap();
                let mut reply = String::new();
                BufReader::new(&stream).read_line(&mut reply).unwrap();
                reply
            })
            .await
            .unwrap();
            assert_eq!(reply, "ping!\n");
            server.await.unwrap();
        }

        #[tokio::test]
        async fn a_busy_pipe_counts_as_served() {
            let path = socket("serving");
            let probe = path.clone();
            assert!(
                !tokio::task::spawn_blocking(move || serving(&probe))
                    .await
                    .unwrap()
            );
            let _listener = Listener::bind(&path).unwrap();
            // NOTE: holds the only instance; nothing accepts it
            let held = Stream::connect(&path).unwrap();
            let probe = path.clone();
            assert!(
                tokio::task::spawn_blocking(move || serving(&probe))
                    .await
                    .unwrap()
            );
            drop(held);
        }

        #[tokio::test]
        async fn a_silent_server_times_the_read_out() {
            let path = socket("silent");
            let mut listener = Listener::bind(&path).unwrap();
            let server = tokio::spawn(async move {
                let pipe = listener.accept().await.unwrap();
                tokio::time::sleep(Duration::from_secs(2)).await;
                drop(pipe);
            });
            let error = tokio::task::spawn_blocking(move || {
                let stream = Stream::connect(&path).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .unwrap();
                (&stream).read(&mut [0u8; 16]).unwrap_err()
            })
            .await
            .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            server.await.unwrap();
        }

        #[tokio::test]
        async fn ingest_messages_arrive_whole_and_in_order() {
            let path = socket("ingest");
            let mut ingest = Ingest::bind(&path).unwrap();
            let sender = tokio::task::spawn_blocking(move || {
                for n in 0..20 {
                    send(&path, format!("message {n}").as_bytes());
                }
            });
            let mut buf = vec![0u8; 64];
            for n in 0..20 {
                let len = ingest.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..len], format!("message {n}").as_bytes());
            }
            sender.await.unwrap();
        }

        #[tokio::test]
        async fn hooks_do_not_wait_on_a_busy_collector() {
            let path = socket("busy");
            let mut ingest = Ingest::bind(&path).unwrap();
            // NOTE: nothing calls recv while the hooks send
            let took = tokio::task::spawn_blocking(move || {
                let start = Instant::now();
                for n in 0..50 {
                    send(&path, format!("m{n}").as_bytes());
                }
                start.elapsed()
            })
            .await
            .unwrap();
            assert!(took < Duration::from_secs(2), "{took:?}");
            let mut buf = vec![0u8; 64];
            for n in 0..50 {
                let len = ingest.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..len], format!("m{n}").as_bytes());
            }
        }

        #[tokio::test]
        async fn a_message_of_the_largest_size_arrives_whole() {
            let path = socket("large");
            let mut ingest = Ingest::bind(&path).unwrap();
            let big = vec![b'x'; crate::wire::MAX_DATAGRAM];
            let sent = big.clone();
            tokio::task::spawn_blocking(move || send(&path, &sent))
                .await
                .unwrap();
            let mut buf = vec![0u8; crate::wire::MAX_DATAGRAM + 1024];
            let len = ingest.recv(&mut buf).await.unwrap();
            assert_eq!(&buf[..len], &big[..]);
        }

        #[tokio::test]
        async fn hooks_at_once_all_get_through() {
            let path = socket("burst");
            let mut ingest = Ingest::bind(&path).unwrap();
            let senders: Vec<_> = (0..16)
                .map(|t| {
                    let path = path.clone();
                    std::thread::spawn(move || {
                        for n in 0..5 {
                            send(&path, format!("{t}.{n}").as_bytes());
                        }
                    })
                })
                .collect();
            let mut got = std::collections::BTreeSet::new();
            let mut buf = vec![0u8; 64];
            while got.len() < 80 {
                let len = tokio::time::timeout(Duration::from_secs(5), ingest.recv(&mut buf))
                    .await
                    .expect("a message was lost")
                    .unwrap();
                got.insert(String::from_utf8_lossy(&buf[..len]).into_owned());
            }
            for sender in senders {
                sender.join().unwrap();
            }
        }
    }
}
