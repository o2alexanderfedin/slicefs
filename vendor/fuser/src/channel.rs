use std::io;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

use nix::errno::Errno;

use crate::dev_fuse::DevFuse;
use crate::passthrough::BackingId;

/// A raw communication channel to the FUSE kernel driver
///
/// The second field is `true` when the fd is a stream socket rather than a
/// character device. FUSE-T (a kext-less FUSE for macOS) hands libfuse a
/// `SOCK_STREAM` socket: unlike `/dev/fuse` / `/dev/macfuse`, a read may return
/// part of a request and a write may be partial, so both directions need framing.
#[derive(Debug, Clone)]
pub(crate) struct Channel(Arc<DevFuse>, bool);

/// Returns true if the fd is a socket (FUSE-T) rather than a FUSE device.
fn is_stream_socket(dev: &DevFuse) -> bool {
    use std::os::unix::fs::FileTypeExt;
    dev.0
        .metadata()
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

/// Read exactly `buf.len()` bytes. Returns `Ok(false)` on EOF before the first byte.
fn read_exact_or_eof(fd: &DevFuse, buf: &mut [u8]) -> nix::Result<bool> {
    let mut off = 0;
    while off < buf.len() {
        match nix::unistd::read(fd, &mut buf[off..]) {
            Ok(0) if off == 0 => return Ok(false),
            Ok(0) => return Err(Errno::EIO),
            Ok(n) => off += n,
            Err(Errno::EINTR | Errno::EAGAIN) => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(true)
}

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
        let stream = is_stream_socket(&device);
        Self(device, stream)
    }

    /// Receives data up to the capacity of the given buffer (can block).
    fn receive(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        if self.1 {
            return self.receive_framed(buffer);
        }
        nix::unistd::read(&self.0, buffer)
    }

    /// Read one whole request from a stream socket: the fixed-size header first,
    /// then exactly `header.len - header_size` more bytes. EOF (peer closed the
    /// socket, e.g. on unmount) is reported as `ENODEV`, which ends the session
    /// cleanly, like a device read after unmount.
    fn receive_framed(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        const HDR: usize = std::mem::size_of::<crate::ll::fuse_abi::fuse_in_header>();
        if buffer.len() < HDR {
            return Err(Errno::EINVAL);
        }
        if !read_exact_or_eof(&self.0, &mut buffer[..HDR])? {
            return Err(Errno::ENODEV);
        }
        // `len` is the first u32 of fuse_in_header: total request size incl. header.
        let len = u32::from_ne_bytes(buffer[..4].try_into().expect("4 bytes")) as usize;
        if len < HDR || len > buffer.len() {
            return Err(Errno::EIO);
        }
        if len > HDR && !read_exact_or_eof(&self.0, &mut buffer[HDR..len])? {
            return Err(Errno::EIO);
        }
        Ok(len)
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

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub(crate) fn sender(&self) -> ChannelSender {
        // Since write/writev syscalls are threadsafe, we can simply create
        // a sender by using the same file and use it in other threads.
        ChannelSender(self.0.clone(), self.1)
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
pub(crate) struct ChannelSender(Arc<DevFuse>, bool);

/// Serializes replies on a stream socket so concurrent senders cannot interleave bytes.
static STREAM_SEND_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl ChannelSender {
    pub(crate) fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        if self.1 {
            return self.send_stream(bufs);
        }
        let rc = nix::sys::uio::writev(&self.0, bufs)?;
        // writev is atomic, so do not need to check how many bytes are written.
        // libfuse does not do it either
        // https://github.com/libfuse/libfuse/blob/6278995cca991978abd25ebb2c20ebd3fc9e8a13/lib/fuse_lowlevel.c#L267
        debug_assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), rc);
        Ok(())
    }

    /// Write a whole reply to a stream socket (writev may be partial there).
    fn send_stream(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        let _guard = STREAM_SEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let total: usize = bufs.iter().map(|b| b.len()).sum();
        let written = loop {
            match nix::sys::uio::writev(&self.0, bufs) {
                Ok(n) => break n,
                Err(Errno::EINTR | Errno::EAGAIN) => continue,
                Err(err) => return Err(err.into()),
            }
        };
        if written == total {
            return Ok(());
        }
        // Partial write: send the rest from a flattened copy.
        let rest: Vec<u8> = bufs.iter().flat_map(|b| b.iter().copied()).skip(written).collect();
        let mut off = 0;
        while off < rest.len() {
            match nix::unistd::write(&self.0, &rest[off..]) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => off += n,
                Err(Errno::EINTR | Errno::EAGAIN) => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    pub(crate) fn open_backing(&self, fd: BorrowedFd<'_>) -> std::io::Result<BackingId> {
        BackingId::create(&self.0, fd)
    }

    pub(crate) unsafe fn wrap_backing(&self, id: u32) -> BackingId {
        unsafe { BackingId::wrap_raw(&self.0, id) }
    }
}
