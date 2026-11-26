use std::io;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

use nix::errno::Errno;
use nix::sys::socket::SockType;
use nix::sys::socket::getsockopt;
use nix::sys::socket::sockopt;
use parking_lot::Mutex;

use crate::dev_fuse::DevFuse;
use crate::ll::fuse_abi::fuse_in_header;
use crate::passthrough::BackingId;

/// A raw communication channel to the FUSE kernel driver
#[derive(Debug, Clone)]
pub(crate) struct Channel(Arc<DevFuse>);

impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Channel {
    /// Create a new communication channel to the kernel driver by mounting the
    /// given path. The kernel driver will delegate filesystem operations of
    /// the given path to the channel.
    pub(crate) fn new(device: Arc<DevFuse>) -> Self {
        Self(device)
    }

    /// Receives data up to the capacity of the given buffer (can block).
    fn receive(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        if let Some(stream) = &self.0.stream_socket {
            return stream.receive(&self.0, buffer);
        }
        nix::unistd::read(&self.0, buffer)
    }

    /// Receives data up to the capacity of the given buffer (can block),
    /// retrying on errors that are safe to retry (ENOENT, EINTR, EAGAIN).
    ///
    /// - ENOENT: Operation interrupted. According to FUSE, this is safe to retry.
    /// - EINTR: Interrupted system call, retry.
    /// - EAGAIN: Explicitly instructed to try again.
    pub(crate) fn receive_retrying(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        loop {
            match self.receive(buffer) {
                Ok(size) => return Ok(size),
                Err(Errno::ENOENT | Errno::EINTR | Errno::EAGAIN) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    /// Returns the FUSE device of this channel.
    pub(crate) fn device(&self) -> Arc<DevFuse> {
        self.0.clone()
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub(crate) fn sender(&self) -> ChannelSender {
        // Since write/writev syscalls are threadsafe, we can simply create
        // a sender by using the same file and use it in other threads.
        ChannelSender(self.0.clone())
    }

    /// Clone the FUSE device fd using FUSE_DEV_IOC_CLONE ioctl.
    ///
    /// This creates a new fd that can read FUSE requests independently,
    /// enabling true parallel request processing. The kernel distributes
    /// requests across all cloned fds.
    ///
    /// Requires Linux 4.5+. Returns an error on older kernels or non-Linux.
    #[cfg(target_os = "linux")]
    pub(crate) fn clone_fd(&self) -> io::Result<Channel> {
        use std::os::fd::AsRawFd;

        let new_dev = DevFuse::open()?;

        let mut source_fd = self.0.as_raw_fd() as u32;
        // SAFETY: fuse_dev_ioc_clone is a valid ioctl for /dev/fuse
        unsafe {
            crate::ll::ioctl::fuse_dev_ioc_clone(new_dev.as_raw_fd(), &mut source_fd)?;
        }

        Ok(Channel::new(Arc::new(new_dev)))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelSender(Arc<DevFuse>);

impl ChannelSender {
    pub(crate) fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        if let Some(stream) = &self.0.stream_socket {
            return stream.send(bufs, |bufs| nix::sys::uio::writev(&self.0, bufs));
        }
        let rc = nix::sys::uio::writev(&self.0, bufs)?;
        // writev is atomic, so do not need to check how many bytes are written.
        // libfuse does not do it either
        // https://github.com/libfuse/libfuse/blob/6278995cca991978abd25ebb2c20ebd3fc9e8a13/lib/fuse_lowlevel.c#L267
        debug_assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), rc);
        Ok(())
    }

    pub(crate) fn open_backing(&self, fd: BorrowedFd<'_>) -> std::io::Result<BackingId> {
        BackingId::create(&self.0, fd)
    }

    pub(crate) unsafe fn wrap_backing(&self, id: u32) -> BackingId {
        unsafe { BackingId::wrap_raw(&self.0, id) }
    }
}

/// Framing for a connection that is a `SOCK_STREAM` socket rather than a FUSE device, as with
/// FUSE-T on macOS. A device returns one whole request per read(2) and takes one whole reply per
/// writev(2). A stream socket may split either, so requests are read by the length in their
/// header and replies are written in full. Each direction is serialized, so that concurrent
/// readers or senders cannot interleave their bytes.
#[derive(Debug, Default)]
pub(crate) struct StreamSocket {
    read: Mutex<()>,
    write: Mutex<()>,
}

impl StreamSocket {
    /// `Some` if `fd` is a stream socket. Seqpacket sockets keep message boundaries and are
    /// used as a device is
    pub(crate) fn detect(fd: impl AsFd) -> Option<Self> {
        match getsockopt(&fd, sockopt::SockType) {
            Ok(SockType::Stream) => Some(Self::default()),
            _ => None,
        }
    }

    /// Receive one whole request. EOF before a request means the peer closed the connection,
    /// as on unmount, and is reported as `ENODEV`, as a device does
    fn receive(&self, fd: impl AsFd, buffer: &mut [u8]) -> nix::Result<usize> {
        const HEADER: usize = size_of::<fuse_in_header>();
        let _guard = self.read.lock();
        let header = buffer.get_mut(..HEADER).ok_or(Errno::EINVAL)?;
        if !read_exact(&fd, header)? {
            return Err(Errno::ENODEV);
        }
        // `len` is the first field of the header, and includes the header itself
        let len = u32::from_ne_bytes(buffer[..4].try_into().unwrap()) as usize;
        // The rest of the stream cannot be found without a valid length, so this is fatal
        let body = buffer.get_mut(HEADER..len).ok_or(Errno::EIO)?;
        if !read_exact(&fd, body)? {
            return Err(Errno::EIO);
        }
        Ok(len)
    }

    /// Send one whole reply, calling `write` until all of it is written
    fn send(
        &self,
        bufs: &[io::IoSlice<'_>],
        mut write: impl FnMut(&[io::IoSlice<'_>]) -> nix::Result<usize>,
    ) -> io::Result<()> {
        let _guard = self.write.lock();
        let mut remaining = bufs.to_vec();
        let mut remaining = &mut remaining[..];
        while !remaining.is_empty() {
            match write(remaining) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => io::IoSlice::advance_slices(&mut remaining, n),
                Err(Errno::EINTR) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }
}

/// Fill `buf` from `fd`. Returns `Ok(false)` on EOF before the first byte
fn read_exact(fd: impl AsFd, buf: &mut [u8]) -> nix::Result<bool> {
    let mut done = 0;
    while done < buf.len() {
        match nix::unistd::read(&fd, &mut buf[done..]) {
            Ok(0) if done == 0 => return Ok(false),
            Ok(0) => return Err(Errno::EIO),
            Ok(n) => done += n,
            Err(Errno::EINTR) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod test {
    use std::io::IoSlice;
    use std::io::Read;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;

    use nix::errno::Errno;

    use super::Channel;
    use super::StreamSocket;

    /// A request with a body of `body_len` bytes, as the kernel would send it
    fn request(body_len: usize) -> Vec<u8> {
        let mut data: Vec<u8> = (0..40 + body_len).map(|i| i as u8).collect();
        data[0..4].copy_from_slice(&((40 + body_len) as u32).to_ne_bytes());
        data
    }

    /// A channel whose kernel end is `kernel`
    fn socket_channel() -> (UnixStream, Channel) {
        let (kernel, device) = UnixStream::pair().unwrap();
        let device = crate::dev_fuse::DevFuse::new(std::fs::File::from(OwnedFd::from(device)));
        (kernel, Channel::new(Arc::new(device)))
    }

    #[test]
    fn stream_socket_is_detected() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert!(StreamSocket::detect(&a).is_some());
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(StreamSocket::detect(&file).is_none());
    }

    #[test]
    fn stream_request_split_across_writes_is_received_whole() {
        let (mut kernel, ch) = socket_channel();
        let first = request(100_000);
        let second = request(0);
        let mut sent = first.clone();
        sent.extend_from_slice(&second);
        let writer = std::thread::spawn(move || {
            for piece in [&sent[..10], &sent[10..40], &sent[40..5000], &sent[5000..]] {
                kernel.write_all(piece).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
        let mut buf = vec![0u8; 200_000];
        let len = ch.receive_retrying(&mut buf).unwrap();
        assert_eq!(&buf[..len], &first[..]);
        let len = ch.receive_retrying(&mut buf).unwrap();
        assert_eq!(&buf[..len], &second[..]);
        writer.join().unwrap();
        // The peer closed the connection
        assert_eq!(ch.receive_retrying(&mut buf), Err(Errno::ENODEV));
    }

    #[test]
    fn stream_eof_inside_a_request_is_an_error() {
        let (mut kernel, device) = UnixStream::pair().unwrap();
        let stream = StreamSocket::detect(&device).unwrap();
        kernel.write_all(&request(100)[..60]).unwrap();
        drop(kernel);
        let mut buf = vec![0u8; 1000];
        assert_eq!(stream.receive(&device, &mut buf), Err(Errno::EIO));
    }

    #[test]
    fn stream_reply_is_written_whole_after_short_writes() {
        let stream = StreamSocket::default();
        let parts: [&[u8]; 3] = [b"header", b"", b"payload"];
        let bufs = parts.map(IoSlice::new);
        let mut written = Vec::new();
        stream
            .send(&bufs, |bufs| {
                // At most 4 bytes per call
                let n = bufs[0].len().min(4);
                written.extend_from_slice(&bufs[0][..n]);
                Ok(n)
            })
            .unwrap();
        assert_eq!(written, b"headerpayload");
    }

    #[test]
    fn stream_reply_larger_than_the_socket_buffer_is_written_whole() {
        let (mut kernel, ch) = socket_channel();
        let reply = request(4 << 20);
        let reader = std::thread::spawn(move || {
            let mut received = Vec::new();
            kernel.read_to_end(&mut received).unwrap();
            received
        });
        let bufs = [IoSlice::new(&reply[..16]), IoSlice::new(&reply[16..])];
        ch.sender().send(&bufs).unwrap();
        drop(ch);
        assert!(reader.join().unwrap() == reply);
    }
}
