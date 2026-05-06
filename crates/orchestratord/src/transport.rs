//! Line-delimited JSON framing.
//!
//! One JSON object per line, terminated with `\n`. Trivial to drive from
//! any client (printf, jq, socat) and trivial to debug with a tail.
//! No length-prefix, no chunking — guests are not pumping megabytes here.

use std::io::{self, BufRead, Write};
use std::mem::size_of;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::protocol::{Request, Response};

pub const DEFAULT_VSOCK_PORT: u32 = 47_000;
pub const VSOCK_HOST_CID: u32 = 2;
pub const VSOCK_CID_ANY: u32 = u32::MAX;

/// Minimal AF_VSOCK listener wrapper. We cannot use `UnixListener`
/// here because its `accept()` path expects a Unix-domain peer address
/// and will error when handed a vsock fd.
pub struct VsockListener {
    fd: RawFd,
}

/// Read one framed request.
///
/// Returns:
///   - `Ok(None)` on clean EOF.
///   - `Ok(Some(Ok(req)))` on a parsed request.
///   - `Ok(Some(Err(e)))` on a malformed line — caller decides whether to
///     reply with -32700 PARSE_ERROR or close.
///   - `Err(io)` only for actual transport faults.
pub fn read_request<R: BufRead>(
    reader: &mut R,
) -> io::Result<Option<Result<Request, serde_json::Error>>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    // Tolerate blank keepalive lines without dropping the connection.
    if line.trim().is_empty() {
        return read_request(reader);
    }
    Ok(Some(serde_json::from_str::<Request>(&line)))
}

/// Serialize, write, terminate, flush. One syscall path per response so
/// readers always see whole frames.
pub fn write_response<W: Write>(writer: &mut W, resp: &Response) -> io::Result<()> {
    let mut buf = serde_json::to_vec(resp)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    buf.push(b'\n');
    writer.write_all(&buf)?;
    writer.flush()
}

pub fn connect_vsock(cid: u32, port: u32) -> io::Result<UnixStream> {
    let fd = socket_vsock()?;
    let addr = sockaddr_vsock(cid, port);
    let rc = unsafe {
        libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

pub fn bind_vsock_listener(port: u32) -> io::Result<VsockListener> {
    let fd = socket_vsock()?;
    let addr = sockaddr_vsock(VSOCK_CID_ANY, port);
    let bind_rc = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if bind_rc != 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let listen_rc = unsafe { libc::listen(fd, 128) };
    if listen_rc != 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(VsockListener { fd })
}

impl VsockListener {
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(self.fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let new_flags = if nonblocking {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        if unsafe { libc::fcntl(self.fd, libc::F_SETFL, new_flags) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn accept(&self) -> io::Result<UnixStream> {
        let fd = unsafe { libc::accept(self.fd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { UnixStream::from_raw_fd(fd) })
    }
}

impl Drop for VsockListener {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}

fn socket_vsock() -> io::Result<RawFd> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn sockaddr_vsock(cid: u32, port: u32) -> libc::sockaddr_vm {
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = cid;
    addr.svm_port = port;
    addr
}
