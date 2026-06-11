// Copyright 2026 Zach Campbell
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use crate::tty::poll_readable;

const O_RDWR: i32 = 0o2;
const O_NOCTTY: i32 = 0o400;
const O_CLOEXEC: i32 = 0o2000000;

const TIOCSCTTY: u64 = 0x540E;
const TIOCSWINSZ: u64 = 0x5414;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

unsafe extern "C" {
    fn posix_openpt(flags: i32) -> i32;
    fn grantpt(fd: i32) -> i32;
    fn unlockpt(fd: i32) -> i32;
    fn ptsname_r(fd: i32, buffer: *mut std::ffi::c_char, len: usize) -> i32;
    fn setsid() -> i32;
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

// Hard ceilings on terminal dimensions, enforced in `PtySize::new`
// (which every size on the wire and in the layout funnels through).
// Each pane's alternate screen allocates rows x cols cells eagerly,
// so without a cap a client lying about its terminal size on attach
// (dims are u16 on the wire) can request a 65535x65535 grid — about
// four billion cells per pane — and OOM the daemon. 512x1024 is ~4x
// the largest real terminal in each axis while bounding a pane's
// grid to half a million cells.
pub const MAX_PTY_ROWS: u16 = 512;
pub const MAX_PTY_COLS: u16 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

impl PtySize {
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows: if rows > MAX_PTY_ROWS { MAX_PTY_ROWS } else { rows },
            cols: if cols > MAX_PTY_COLS { MAX_PTY_COLS } else { cols },
        }
    }
}

#[derive(Debug)]
pub struct PtyProcess {
    child: Child,
    reader: File,
    writer: File,
}

impl PtyProcess {
    pub fn spawn(program: &str, args: &[&str], size: PtySize) -> io::Result<Self> {
        let master = open_master()?;
        set_winsize(master.as_raw_fd(), size)?;

        let slave_path = query_slave_path(master.as_raw_fd())?;
        // O_NOCTTY: don't let this open() steal the PTY as our controlling
        // terminal. If the parent is a session leader with no ctty (e.g. the
        // daemon spawned under setsid), a naive open would silently claim the
        // slave, and the forked child's TIOCSCTTY would then fail with EPERM
        // because the PTY is already owned by the parent's session.
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(&slave_path)?;
        set_winsize(slave.as_raw_fd(), size)?;

        let stdin_file = slave.try_clone()?;
        let stdout_file = slave.try_clone()?;
        let stderr_file = slave.try_clone()?;
        let slave_fd = slave.as_raw_fd();

        let mut command = Command::new(program);
        command.args(args);
        command.env("TERM", "xterm-256color");
        command.stdin(Stdio::from(stdin_file));
        command.stdout(Stdio::from(stdout_file));
        command.stderr(Stdio::from(stderr_file));

        // The child must become a session leader before claiming the PTY slave
        // as its controlling terminal.
        unsafe {
            command.pre_exec(move || {
                if setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }

                if ioctl(slave_fd, TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }

                Ok(())
            });
        }

        let child = command.spawn()?;
        drop(slave);

        let reader = master.try_clone()?;
        let writer = master;

        Ok(Self {
            child,
            reader,
            writer,
        })
    }

    pub fn read_to_end(&mut self) -> io::Result<Vec<u8>> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match self.reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => buffer.extend_from_slice(&chunk[..count]),
                // Linux PTY masters often report EIO when the slave closes.
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(buffer)
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    pub fn read_available(&mut self) -> io::Result<Vec<u8>> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];

        while poll_readable(self.reader.as_raw_fd(), 0)? {
            match self.reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => buffer.extend_from_slice(&chunk[..count]),
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => return Err(error),
            }
        }

        Ok(buffer)
    }

    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        set_winsize(self.writer.as_raw_fd(), size)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    pub fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait()
    }

    // Force the child down and reap it. Used when a user closes a pane so we
    // don't leave a zombie shell behind. Idempotent if the child has already
    // exited — `kill` returns InvalidInput for a reaped child, which we
    // ignore.
    pub fn kill(&mut self) -> io::Result<std::process::ExitStatus> {
        let _ = self.child.kill();
        self.child.wait()
    }
}

fn open_master() -> io::Result<File> {
    let fd = unsafe { posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }

    if unsafe { grantpt(fd) } == -1 {
        return Err(io::Error::last_os_error());
    }

    if unsafe { unlockpt(fd) } == -1 {
        return Err(io::Error::last_os_error());
    }

    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(File::from(owned))
}

fn query_slave_path(master_fd: i32) -> io::Result<String> {
    let mut buffer = [0i8; 128];
    let result = unsafe { ptsname_r(master_fd, buffer.as_mut_ptr(), buffer.len()) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }

    let cstr = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    Ok(cstr.to_string_lossy().into_owned())
}

fn set_winsize(fd: i32, size: PtySize) -> io::Result<()> {
    let winsize = Winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let result = unsafe { ioctl(fd, TIOCSWINSZ, &winsize) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PtyProcess, PtySize};

    #[test]
    fn can_spawn_a_command_inside_a_pty() {
        let mut process = PtyProcess::spawn(
            "/bin/sh",
            &["-lc", "printf 'alpha\\nbeta\\n'"],
            PtySize::new(24, 80),
        )
        .expect("spawn PTY process");

        let bytes = process.read_to_end().expect("read PTY output");
        let status = process.wait().expect("wait for PTY child");

        let output = String::from_utf8_lossy(&bytes);
        assert!(status.success());
        assert!(output.contains("alpha\r\n"));
        assert!(output.contains("beta\r\n"));
    }

    #[test]
    fn kill_on_an_already_reaped_child_is_a_noop() {
        // Mirrors the close_active path: update_exit_statuses has already
        // called try_wait and reaped the child by the time close_active
        // invokes kill(). A second kill + wait must not panic and must
        // still return the cached exit status.
        let mut process = PtyProcess::spawn("/bin/sh", &["-lc", "exit 0"], PtySize::new(24, 80))
            .expect("spawn PTY process");

        let status_first = process.wait().expect("initial wait");
        assert!(status_first.success());

        // On Linux child.kill() after reap returns InvalidInput; we
        // deliberately discard that error, and wait() returns the cached
        // status. What we're guarding against is a panic or unrelated
        // error escaping.
        let status_second = process.kill().expect("kill-then-wait on reaped child");
        assert_eq!(status_second.code(), status_first.code());
    }
}
