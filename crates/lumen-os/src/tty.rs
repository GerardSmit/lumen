//! Terminals: termios attributes, break / drain / flush / flow, window size and pseudo-terminals
//! (`openpty`, `forkpty`, `login_tty`). The Python `termios`, `tty`, `pty` and `os.openpty`
//! family runs on these; Node's raw mode uses [`set_raw`]. Off Unix everything fails with
//! `ENOSYS`.

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

#[cfg(unix)]
fn check(rc: libc::c_int) -> R<libc::c_int> {
    if rc < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(rc)
    }
}

/// A terminal's mode (`struct termios`) with the flag words widened to 64 bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Termios {
    pub iflag: u64,
    pub oflag: u64,
    pub cflag: u64,
    pub lflag: u64,
    pub ispeed: u64,
    pub ospeed: u64,
    /// The `NCCS` control characters.
    pub cc: Vec<u8>,
}

/// Where `VMIN` and `VTIME` sit in `cc`, and the `ICANON` bit of `lflag`: `(vmin, vtime, icanon)`.
pub fn noncanonical_slots() -> (usize, usize, u64) {
    #[cfg(unix)]
    {
        (libc::VMIN, libc::VTIME, libc::ICANON as u64)
    }
    #[cfg(not(unix))]
    {
        (0, 0, 0)
    }
}

/// The number of control characters (`NCCS`).
pub fn nccs() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: a zeroed termios is a valid value to take the size of.
        let t: libc::termios = unsafe { std::mem::zeroed() };
        t.c_cc.len()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(unix)]
fn get(fd: i32) -> R<libc::termios> {
    // SAFETY: a zeroed termios is a valid out-parameter (some libcs leave fields unset).
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    check(unsafe { libc::tcgetattr(fd, &mut t) })?;
    Ok(t)
}

/// `tcgetattr(3)`.
pub fn tcgetattr(fd: i32) -> R<Termios> {
    #[cfg(unix)]
    {
        let t = get(fd)?;
        // SAFETY: `t` is a live termios.
        let (ispeed, ospeed) = unsafe { (libc::cfgetispeed(&t), libc::cfgetospeed(&t)) };
        Ok(Termios {
            iflag: t.c_iflag as u64,
            oflag: t.c_oflag as u64,
            cflag: t.c_cflag as u64,
            lflag: t.c_lflag as u64,
            ispeed: ispeed as u64,
            ospeed: ospeed as u64,
            cc: t.c_cc.iter().map(|&c| c as u8).collect(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        Err(FsError("ENOSYS"))
    }
}

/// `tcsetattr(3)` (`when`: `TCSANOW`, `TCSADRAIN`, `TCSAFLUSH`). The current mode is read first so
/// fields this type does not carry keep their values.
pub fn tcsetattr(fd: i32, when: i32, mode: &Termios) -> R<()> {
    #[cfg(unix)]
    {
        let mut t = get(fd)?;
        t.c_iflag = mode.iflag as _;
        t.c_oflag = mode.oflag as _;
        t.c_cflag = mode.cflag as _;
        t.c_lflag = mode.lflag as _;
        for (slot, &c) in t.c_cc.iter_mut().zip(&mode.cc) {
            *slot = c as _;
        }
        // SAFETY: `t` is a live termios.
        unsafe {
            check(libc::cfsetispeed(&mut t, mode.ispeed as _))?;
            check(libc::cfsetospeed(&mut t, mode.ospeed as _))?;
            check(libc::tcsetattr(fd, when, &t)).map(|_| ())
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, when, mode);
        Err(FsError("ENOSYS"))
    }
}

/// `cfmakeraw` of the terminal's current mode: the settings raw mode needs, without applying
/// them. Returns the mode to restore.
pub fn set_raw(fd: i32, raw: bool, saved: Option<&Termios>) -> R<Termios> {
    #[cfg(unix)]
    {
        let before = tcgetattr(fd)?;
        if !raw {
            if let Some(saved) = saved {
                tcsetattr(fd, libc::TCSADRAIN, saved)?;
            }
            return Ok(before);
        }
        let mut t = get(fd)?;
        // SAFETY: `t` is a live termios.
        unsafe { libc::cfmakeraw(&mut t) };
        // SAFETY: `t` is a live termios.
        check(unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, &t) })?;
        Ok(before)
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, raw, saved);
        Err(FsError("ENOSYS"))
    }
}

pub fn tcsendbreak(fd: i32, duration: i32) -> R<()> {
    #[cfg(unix)]
    {
        // SAFETY: plain integer arguments.
        check(unsafe { libc::tcsendbreak(fd, duration) }).map(|_| ())
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, duration);
        Err(FsError("ENOSYS"))
    }
}

pub fn tcdrain(fd: i32) -> R<()> {
    #[cfg(unix)]
    {
        // SAFETY: plain integer argument.
        check(unsafe { libc::tcdrain(fd) }).map(|_| ())
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        Err(FsError("ENOSYS"))
    }
}

pub fn tcflush(fd: i32, queue: i32) -> R<()> {
    #[cfg(unix)]
    {
        // SAFETY: plain integer arguments.
        check(unsafe { libc::tcflush(fd, queue) }).map(|_| ())
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, queue);
        Err(FsError("ENOSYS"))
    }
}

pub fn tcflow(fd: i32, action: i32) -> R<()> {
    #[cfg(unix)]
    {
        // SAFETY: plain integer arguments.
        check(unsafe { libc::tcflow(fd, action) }).map(|_| ())
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, action);
        Err(FsError("ENOSYS"))
    }
}

/// `(rows, columns)` of the terminal behind `fd` (`TIOCGWINSZ`).
pub fn get_winsize(fd: i32) -> R<(u16, u16)> {
    #[cfg(unix)]
    {
        // SAFETY: a zeroed winsize is a valid out-parameter for TIOCGWINSZ.
        let mut w: libc::winsize = unsafe { std::mem::zeroed() };
        check(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut w) })?;
        Ok((w.ws_row, w.ws_col))
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        Err(FsError("ENOSYS"))
    }
}

/// Sets the rows and columns of the terminal behind `fd`, keeping its pixel sizes.
pub fn set_winsize(fd: i32, rows: u16, cols: u16) -> R<()> {
    #[cfg(unix)]
    {
        // SAFETY: a zeroed winsize is a valid out-parameter for TIOCGWINSZ.
        let mut w: libc::winsize = unsafe { std::mem::zeroed() };
        check(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut w) })?;
        w.ws_row = rows;
        w.ws_col = cols;
        check(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &w) }).map(|_| ())
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, rows, cols);
        Err(FsError("ENOSYS"))
    }
}

/// `openpty(3)`: `(master, slave)`, both close-on-exec.
pub fn openpty() -> R<(i32, i32)> {
    #[cfg(unix)]
    {
        let (mut master, mut slave) = (0 as libc::c_int, 0 as libc::c_int);
        // SAFETY: both out-parameters are live; the name, termios and winsize are optional.
        check(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) })?;
        for fd in [master, slave] {
            if let Err(e) = crate::fdctl::set_inheritable(fd, false) {
                // SAFETY: both descriptors are fresh and owned here.
                unsafe {
                    libc::close(master);
                    libc::close(slave);
                }
                return Err(e);
            }
        }
        Ok((master, slave))
    }
    #[cfg(not(unix))]
    Err(FsError("ENOSYS"))
}

/// `forkpty(3)`: `(pid, master)`; `pid` is 0 in the child, whose stdio is the new terminal.
pub fn forkpty() -> R<(i32, i32)> {
    #[cfg(unix)]
    {
        let mut master: libc::c_int = 0;
        // SAFETY: the out-parameter is live; the name, termios and winsize are optional.
        let pid = check(unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) })?;
        if pid != 0 {
            crate::fdctl::set_inheritable(master, false)?;
        }
        Ok((pid, master))
    }
    #[cfg(not(unix))]
    Err(FsError("ENOSYS"))
}

/// `login_tty(3)`: starts a session, makes `fd` its controlling terminal and its stdio, and
/// closes `fd` when it is above 2.
pub fn login_tty(fd: i32) -> R<()> {
    #[cfg(unix)]
    {
        // SAFETY: plain descriptor syscalls; the kernel validates every number.
        unsafe {
            check(libc::setsid())?;
            check(libc::ioctl(fd, libc::TIOCSCTTY as _, 0))?;
            for target in 0..3 {
                check(libc::dup2(fd, target))?;
            }
            if fd > 2 {
                check(libc::close(fd))?;
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        Err(FsError("ENOSYS"))
    }
}

/// The `termios` module's constants on this platform (flags, speeds, control-character indexes,
/// `ioctl` requests).
pub fn constants() -> Vec<(&'static str, i64)> {
    let mut v: Vec<(&'static str, i64)> = Vec::new();
    #[cfg(unix)]
    v.push(("NCCS", nccs() as i64));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    v.extend(LINUX);
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    v.extend(MACOS);
    v
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const LINUX: &[(&str, i64)] = &[
    ("B0", 0),
    ("B50", 1),
    ("B75", 2),
    ("B110", 3),
    ("B134", 4),
    ("B150", 5),
    ("B200", 6),
    ("B300", 7),
    ("B600", 8),
    ("B1200", 9),
    ("B1800", 10),
    ("B2400", 11),
    ("B4800", 12),
    ("B9600", 13),
    ("B19200", 14),
    ("B38400", 15),
    ("B57600", 0x1001),
    ("B115200", 0x1002),
    ("B230400", 0x1003),
    ("B460800", 0x1004),
    ("B500000", 0x1005),
    ("B576000", 0x1006),
    ("B921600", 0x1007),
    ("B1000000", 0x1008),
    ("B1152000", 0x1009),
    ("B1500000", 0x100a),
    ("B2000000", 0x100b),
    ("B2500000", 0x100c),
    ("B3000000", 0x100d),
    ("B3500000", 0x100e),
    ("B4000000", 0x100f),
    ("EXTA", 14),
    ("EXTB", 15),
    ("CBAUD", 0x100f),
    ("CBAUDEX", 0x1000),
    ("CIBAUD", 0x100f_0000),
    ("TCSANOW", 0),
    ("TCSADRAIN", 1),
    ("TCSAFLUSH", 2),
    ("TCIFLUSH", 0),
    ("TCOFLUSH", 1),
    ("TCIOFLUSH", 2),
    ("TCOOFF", 0),
    ("TCOON", 1),
    ("TCIOFF", 2),
    ("TCION", 3),
    ("IGNBRK", 0x1),
    ("BRKINT", 0x2),
    ("IGNPAR", 0x4),
    ("PARMRK", 0x8),
    ("INPCK", 0x10),
    ("ISTRIP", 0x20),
    ("INLCR", 0x40),
    ("IGNCR", 0x80),
    ("ICRNL", 0x100),
    ("IUCLC", 0x200),
    ("IXON", 0x400),
    ("IXANY", 0x800),
    ("IXOFF", 0x1000),
    ("IMAXBEL", 0x2000),
    ("OPOST", 0x1),
    ("OLCUC", 0x2),
    ("ONLCR", 0x4),
    ("OCRNL", 0x8),
    ("ONOCR", 0x10),
    ("ONLRET", 0x20),
    ("OFILL", 0x40),
    ("OFDEL", 0x80),
    ("NLDLY", 0x100),
    ("CRDLY", 0x600),
    ("TABDLY", 0x1800),
    ("BSDLY", 0x2000),
    ("VTDLY", 0x4000),
    ("FFDLY", 0x8000),
    ("NL0", 0),
    ("NL1", 0x100),
    ("CR0", 0),
    ("CR1", 0x200),
    ("CR2", 0x400),
    ("CR3", 0x600),
    ("TAB0", 0),
    ("TAB1", 0x800),
    ("TAB2", 0x1000),
    ("TAB3", 0x1800),
    ("XTABS", 0x1800),
    ("BS0", 0),
    ("BS1", 0x2000),
    ("VT0", 0),
    ("VT1", 0x4000),
    ("FF0", 0),
    ("FF1", 0x8000),
    ("CSIZE", 0x30),
    ("CS5", 0),
    ("CS6", 0x10),
    ("CS7", 0x20),
    ("CS8", 0x30),
    ("CSTOPB", 0x40),
    ("CREAD", 0x80),
    ("PARENB", 0x100),
    ("PARODD", 0x200),
    ("HUPCL", 0x400),
    ("CLOCAL", 0x800),
    ("CRTSCTS", 0x8000_0000),
    ("ISIG", 0x1),
    ("ICANON", 0x2),
    ("XCASE", 0x4),
    ("ECHO", 0x8),
    ("ECHOE", 0x10),
    ("ECHOK", 0x20),
    ("ECHONL", 0x40),
    ("NOFLSH", 0x80),
    ("TOSTOP", 0x100),
    ("ECHOCTL", 0x200),
    ("ECHOPRT", 0x400),
    ("ECHOKE", 0x800),
    ("FLUSHO", 0x1000),
    ("PENDIN", 0x4000),
    ("IEXTEN", 0x8000),
    ("VINTR", 0),
    ("VQUIT", 1),
    ("VERASE", 2),
    ("VKILL", 3),
    ("VEOF", 4),
    ("VTIME", 5),
    ("VMIN", 6),
    ("VSWTC", 7),
    ("VSWTCH", 7),
    ("VSTART", 8),
    ("VSTOP", 9),
    ("VSUSP", 10),
    ("VEOL", 11),
    ("VREPRINT", 12),
    ("VDISCARD", 13),
    ("VWERASE", 14),
    ("VLNEXT", 15),
    ("VEOL2", 16),
    ("NSWTCH", 8),
    ("N_TTY", 0),
    ("N_SLIP", 1),
    ("N_MOUSE", 2),
    ("N_PPP", 3),
    ("N_STRIP", 4),
    ("TCGETS", 0x5401),
    ("TCSETS", 0x5402),
    ("TCSETSW", 0x5403),
    ("TCSETSF", 0x5404),
    ("TCGETA", 0x5405),
    ("TCSETA", 0x5406),
    ("TCSETAW", 0x5407),
    ("TCSETAF", 0x5408),
    ("TCSBRK", 0x5409),
    ("TCXONC", 0x540a),
    ("TCFLSH", 0x540b),
    ("TIOCEXCL", 0x540c),
    ("TIOCNXCL", 0x540d),
    ("TIOCSCTTY", 0x540e),
    ("TIOCGPGRP", 0x540f),
    ("TIOCSPGRP", 0x5410),
    ("TIOCOUTQ", 0x5411),
    ("TIOCSTI", 0x5412),
    ("TIOCGWINSZ", 0x5413),
    ("TIOCSWINSZ", 0x5414),
    ("TIOCMGET", 0x5415),
    ("TIOCMBIS", 0x5416),
    ("TIOCMBIC", 0x5417),
    ("TIOCMSET", 0x5418),
    ("TIOCGSOFTCAR", 0x5419),
    ("TIOCSSOFTCAR", 0x541a),
    ("FIONREAD", 0x541b),
    ("TIOCINQ", 0x541b),
    ("TIOCLINUX", 0x541c),
    ("TIOCCONS", 0x541d),
    ("TIOCGSERIAL", 0x541e),
    ("TIOCSSERIAL", 0x541f),
    ("TIOCPKT", 0x5420),
    ("FIONBIO", 0x5421),
    ("TIOCNOTTY", 0x5422),
    ("TIOCSETD", 0x5423),
    ("TIOCGETD", 0x5424),
    ("TCSBRKP", 0x5425),
    ("IOCSIZE_MASK", 0x3fff_0000),
    ("IOCSIZE_SHIFT", 16),
    ("FIONCLEX", 0x5450),
    ("FIOCLEX", 0x5451),
    ("FIOASYNC", 0x5452),
    ("TIOCSERCONFIG", 0x5453),
    ("TIOCSERGWILD", 0x5454),
    ("TIOCSERSWILD", 0x5455),
    ("TIOCGLCKTRMIOS", 0x5456),
    ("TIOCSLCKTRMIOS", 0x5457),
    ("TIOCSERGSTRUCT", 0x5458),
    ("TIOCSERGETLSR", 0x5459),
    ("TIOCSERGETMULTI", 0x545a),
    ("TIOCSERSETMULTI", 0x545b),
    ("TIOCMIWAIT", 0x545c),
    ("TIOCGICOUNT", 0x545d),
    ("TIOCSER_TEMT", 1),
    ("TIOCM_LE", 0x1),
    ("TIOCM_DTR", 0x2),
    ("TIOCM_RTS", 0x4),
    ("TIOCM_ST", 0x8),
    ("TIOCM_SR", 0x10),
    ("TIOCM_CTS", 0x20),
    ("TIOCM_CAR", 0x40),
    ("TIOCM_CD", 0x40),
    ("TIOCM_RNG", 0x80),
    ("TIOCM_RI", 0x80),
    ("TIOCM_DSR", 0x100),
    ("TIOCPKT_DATA", 0),
    ("TIOCPKT_FLUSHREAD", 1),
    ("TIOCPKT_FLUSHWRITE", 2),
    ("TIOCPKT_STOP", 4),
    ("TIOCPKT_START", 8),
    ("TIOCPKT_NOSTOP", 16),
    ("TIOCPKT_DOSTOP", 32),
];

#[cfg(any(target_os = "macos", target_os = "ios"))]
const MACOS: &[(&str, i64)] = &[
    ("ALTWERASE", 512),
    ("B7200", 7200),
    ("B14400", 14400),
    ("B28800", 28800),
    ("B76800", 76800),
    ("CCAR_OFLOW", 1048576),
    ("CCTS_OFLOW", 65536),
    ("CDSR_OFLOW", 524288),
    ("CDTR_IFLOW", 262144),
    ("CIGNORE", 1),
    ("CRTS_IFLOW", 131072),
    ("EXTPROC", 2048),
    ("IUTF8", 16384),
    ("MDMBUF", 1048576),
    ("NL2", 512),
    ("NL3", 768),
    ("NOKERNINFO", 33554432),
    ("ONOEOT", 8),
    ("OXTABS", 4),
    ("TIOCGSIZE", 1074295912),
    ("TIOCSSIZE", 2148037735),
    ("VDSUSP", 11),
    ("VSTATUS", 18),
    ("_POSIX_VDISABLE", 255),
    ("B0", 0),
    ("B50", 50),
    ("B75", 75),
    ("B110", 110),
    ("B134", 134),
    ("B150", 150),
    ("B200", 200),
    ("B300", 300),
    ("B600", 600),
    ("B1200", 1200),
    ("B1800", 1800),
    ("B2400", 2400),
    ("B4800", 4800),
    ("B9600", 9600),
    ("B19200", 19200),
    ("B38400", 38400),
    ("B57600", 57600),
    ("B115200", 115200),
    ("B230400", 230400),
    ("EXTA", 19200),
    ("EXTB", 38400),
    ("TCSANOW", 0),
    ("TCSADRAIN", 1),
    ("TCSAFLUSH", 2),
    ("TCSASOFT", 0x10),
    ("TCIFLUSH", 1),
    ("TCOFLUSH", 2),
    ("TCIOFLUSH", 3),
    ("TCOOFF", 1),
    ("TCOON", 2),
    ("TCIOFF", 3),
    ("TCION", 4),
    ("IGNBRK", 0x1),
    ("BRKINT", 0x2),
    ("IGNPAR", 0x4),
    ("PARMRK", 0x8),
    ("INPCK", 0x10),
    ("ISTRIP", 0x20),
    ("INLCR", 0x40),
    ("IGNCR", 0x80),
    ("ICRNL", 0x100),
    ("IXON", 0x200),
    ("IXOFF", 0x400),
    ("IXANY", 0x800),
    ("IMAXBEL", 0x2000),
    ("OPOST", 0x1),
    ("ONLCR", 0x2),
    ("OCRNL", 0x10),
    ("ONOCR", 0x20),
    ("ONLRET", 0x40),
    ("OFILL", 0x80),
    ("OFDEL", 0x20000),
    ("NLDLY", 0x300),
    ("TABDLY", 0xc04),
    ("CRDLY", 0x3000),
    ("FFDLY", 0x4000),
    ("BSDLY", 0x8000),
    ("VTDLY", 0x10000),
    ("NL0", 0),
    ("NL1", 0x100),
    ("TAB0", 0),
    ("TAB1", 0x400),
    ("TAB2", 0x800),
    ("TAB3", 0x4),
    ("CR0", 0),
    ("CR1", 0x1000),
    ("CR2", 0x2000),
    ("CR3", 0x3000),
    ("FF0", 0),
    ("FF1", 0x4000),
    ("BS0", 0),
    ("BS1", 0x8000),
    ("VT0", 0),
    ("VT1", 0x10000),
    ("CSIZE", 0x300),
    ("CS5", 0),
    ("CS6", 0x100),
    ("CS7", 0x200),
    ("CS8", 0x300),
    ("CSTOPB", 0x400),
    ("CREAD", 0x800),
    ("PARENB", 0x1000),
    ("PARODD", 0x2000),
    ("HUPCL", 0x4000),
    ("CLOCAL", 0x8000),
    ("CRTSCTS", 0x30000),
    ("ECHOKE", 0x1),
    ("ECHOE", 0x2),
    ("ECHOK", 0x4),
    ("ECHO", 0x8),
    ("ECHONL", 0x10),
    ("ECHOPRT", 0x20),
    ("ECHOCTL", 0x40),
    ("ISIG", 0x80),
    ("ICANON", 0x100),
    ("IEXTEN", 0x400),
    ("TOSTOP", 0x400000),
    ("FLUSHO", 0x800000),
    ("PENDIN", 0x20000000),
    ("NOFLSH", 0x80000000),
    ("VEOF", 0),
    ("VEOL", 1),
    ("VEOL2", 2),
    ("VERASE", 3),
    ("VWERASE", 4),
    ("VKILL", 5),
    ("VREPRINT", 6),
    ("VINTR", 8),
    ("VQUIT", 9),
    ("VSUSP", 10),
    ("VSTART", 12),
    ("VSTOP", 13),
    ("VLNEXT", 14),
    ("VDISCARD", 15),
    ("VMIN", 16),
    ("VTIME", 17),
    ("CEOF", 0x04),
    ("CEOT", 0x04),
    ("CEOL", 0xff),
    ("CERASE", 0x7f),
    ("CINTR", 0x03),
    ("CKILL", 0x15),
    ("CLNEXT", 0x16),
    ("CQUIT", 0x1c),
    ("CRPRNT", 0x12),
    ("CSTART", 0x11),
    ("CSTOP", 0x13),
    ("CSUSP", 0x1a),
    ("CWERASE", 0x17),
    ("CDSUSP", 0x19),
    ("CFLUSH", 0x0f),
    ("TIOCEXCL", 0x2000740d),
    ("TIOCNXCL", 0x2000740e),
    ("TIOCSCTTY", 0x20007461),
    ("TIOCNOTTY", 0x20007471),
    ("TIOCGPGRP", 0x40047477),
    ("TIOCSPGRP", 0x80047476),
    ("TIOCOUTQ", 0x40047473),
    ("TIOCSTI", 0x80017472),
    ("TIOCGWINSZ", 0x40087468),
    ("TIOCSWINSZ", 0x80087467),
    ("TIOCMGET", 0x4004746a),
    ("TIOCMBIC", 0x8004746b),
    ("TIOCMBIS", 0x8004746c),
    ("TIOCMSET", 0x8004746d),
    ("TIOCCONS", 0x80047462),
    ("TIOCPKT", 0x80047470),
    ("TIOCGETD", 0x4004741a),
    ("TIOCSETD", 0x8004741b),
    ("FIONREAD", 0x4004667f),
    ("FIONBIO", 0x8004667e),
    ("FIOASYNC", 0x8004667d),
    ("FIOCLEX", 0x20006601),
    ("FIONCLEX", 0x20006602),
    ("TIOCM_LE", 0x1),
    ("TIOCM_DTR", 0x2),
    ("TIOCM_RTS", 0x4),
    ("TIOCM_ST", 0x8),
    ("TIOCM_SR", 0x10),
    ("TIOCM_CTS", 0x20),
    ("TIOCM_CAR", 0x40),
    ("TIOCM_CD", 0x40),
    ("TIOCM_RNG", 0x80),
    ("TIOCM_RI", 0x80),
    ("TIOCM_DSR", 0x100),
    ("TIOCPKT_DATA", 0),
    ("TIOCPKT_FLUSHREAD", 1),
    ("TIOCPKT_FLUSHWRITE", 2),
    ("TIOCPKT_STOP", 4),
    ("TIOCPKT_START", 8),
    ("TIOCPKT_NOSTOP", 16),
    ("TIOCPKT_DOSTOP", 32),
];

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn pty_attributes_round_trip() {
        let (master, slave) = openpty().unwrap();
        let mut mode = tcgetattr(slave).unwrap();
        assert_eq!(mode.cc.len(), nccs());
        mode.lflag &= !(constants().iter().find(|c| c.0 == "ECHO").unwrap().1 as u64);
        tcsetattr(slave, constants().iter().find(|c| c.0 == "TCSANOW").unwrap().1 as i32, &mode).unwrap();
        assert_eq!(tcgetattr(slave).unwrap().lflag, mode.lflag);
        set_winsize(slave, 24, 80).unwrap();
        assert_eq!(get_winsize(master).unwrap(), (24, 80));
        crate::fs::close(master).unwrap();
        crate::fs::close(slave).unwrap();
    }
}
