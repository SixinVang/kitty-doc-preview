//! Terminal primitives: /dev/tty access, winsize ioctl, a raw-mode guard,
//! and timed query/response reading used for capability probing.

use std::io;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

/// Open the controlling terminal read/write.
pub fn open_tty() -> io::Result<i32> {
    let fd = unsafe {
        libc::open(
            c"/dev/tty".as_ptr() as *const libc::c_char,
            libc::O_RDWR | libc::O_NOCTTY,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

pub fn winsize(fd: i32) -> io::Result<Winsize> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Winsize {
            rows: ws.ws_row,
            cols: ws.ws_col,
            xpixel: ws.ws_xpixel,
            ypixel: ws.ws_ypixel,
        })
    }
}

/// Terminal size via TIOCGWINSZ on /dev/tty.
pub fn tty_winsize() -> io::Result<Winsize> {
    let fd = open_tty()?;
    let r = winsize(fd);
    unsafe { libc::close(fd) };
    r
}

/// Raw mode on the given fd, restored on drop.
pub struct RawGuard {
    fd: i32,
    saved: libc::termios,
}

impl RawGuard {
    pub fn new(fd: i32) -> io::Result<Self> {
        unsafe {
            let mut t = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd, saved })
        }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

/// When `query_tty` should stop reading.
pub enum Stop {
    /// Until a primary device attributes reply (`ESC [ ? ... c`).
    Da1,
    /// Until a complete graphics reply for image `id`.
    Graphics(u32),
}

/// Write `queries` to /dev/tty in raw mode and collect the reply until
/// `stop` is satisfied or `timeout` elapses.
pub fn query_tty(queries: &[u8], stop: Stop, timeout: Duration) -> io::Result<Vec<u8>> {
    let fd = open_tty()?;
    let res = (|| {
        let _raw = RawGuard::new(fd)?;
        unsafe { libc::tcflush(fd, libc::TCIFLUSH) };
        write_all_fd(fd, queries)?;
        read_until(fd, &stop, timeout)
    })();
    unsafe { libc::close(fd) };
    res
}

fn write_all_fd(fd: i32, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n <= 0 {
            return Err(io::Error::last_os_error());
        }
        data = &data[n as usize..];
    }
    Ok(())
}

fn read_until(fd: i32, stop: &Stop, timeout: Duration) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= deadline || stop_met(&buf, stop) {
            break;
        }
        let wait = (deadline - now).as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, wait) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if r == 0 {
            break; // timed out
        }
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            break; // EOF
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
    Ok(buf)
}

fn stop_met(buf: &[u8], stop: &Stop) -> bool {
    match stop {
        Stop::Da1 => find(buf, b"\x1b[?")
            .map(|i| {
                buf[i + 3..]
                    .iter()
                    .take_while(|&&b| b != 0x1b)
                    .any(|&b| b == b'c')
            })
            .unwrap_or(false),
        Stop::Graphics(id) => {
            let prefix = format!("\x1b_Gi={id}");
            find(buf, prefix.as_bytes())
                .map(|i| find(&buf[i..], b"\x1b\\").is_some())
                .unwrap_or(false)
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_da1_detects_reply() {
        assert!(!stop_met(b"\x1b[6;28;14t", &Stop::Da1));
        assert!(stop_met(b"\x1b[6;28;14t\x1b[?62;1;2c", &Stop::Da1));
        // a CSI ending in c without ? must not count
        assert!(!stop_met(b"\x1b[6;28c", &Stop::Da1));
    }

    #[test]
    fn stop_graphics_detects_block() {
        assert!(!stop_met(b"\x1b_Gi=33", &Stop::Graphics(33)));
        assert!(stop_met(b"\x1b_Gi=33;OK\x1b\\", &Stop::Graphics(33)));
        assert!(!stop_met(b"\x1b_Gi=31;OK\x1b\\", &Stop::Graphics(33)));
    }
}
