//! `termios` on `lumen_os::tty`: POSIX terminal attributes, break, drain, flush, flow and the
//! window size, with CPython's `termios.error` for system failures.

/// This module provides an interface to the Posix calls for tty I/O control.
/// For a complete description of these calls, see the Posix or Unix manual
/// pages. It is only available for those Unix versions that support Posix
/// termios style tty I/O control.
///
/// All functions in this module take a file descriptor fd as their first
/// argument. This can be an integer file descriptor, such as returned by
/// sys.stdin.fileno(), or a file object, such as sys.stdin itself.
#[lumen_bind::module(name = "termios")]
pub mod termios {
    use crate::builtins::posixm::as_file_descriptor;
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_os::tty::Termios;
    use lumen_os::FsError;

    #[derive(Default)]
    pub struct State {
        error: Option<Obj>,
    }

    fn tty_error(it: &mut Interp, e: FsError) -> Obj {
        let errno = e.errno();
        let cls = match it.native_state::<State>().error.clone() {
            Some(c) => c,
            None => it.exc_type("Exception"),
        };
        it.new_exc(&cls, vec![Value::Int(errno as i64), Value::string(lumen_os::errno::strerror(errno))])
    }

    fn bytes_of(v: &Value) -> Option<&[u8]> {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Bytes(b) => Some(b),
                _ => None,
            },
            _ => None,
        }
    }

    fn word(it: &mut Interp, v: &Value) -> R<u64> {
        Ok(it.index_of(v)? as u64)
    }

    fn items(v: &Value) -> Option<Vec<Value>> {
        list_of(v).map(|l| l.borrow().clone())
    }

    /// Get the tty attributes for file descriptor fd, as follows:
    /// [iflag, oflag, cflag, lflag, ispeed, ospeed, cc] where cc is a list
    /// of the tty special characters (each a string of length 1, except the
    /// items with indices VMIN and VTIME, which are integers when these
    /// fields are defined).  The interpretation of the flags and the speeds
    /// as well as the indexing in the cc array must be done using the
    /// symbolic constants defined in this module.
    #[op]
    fn tcgetattr(it: &mut Interp, fd: &Value) -> R<Value> {
        let fd = as_file_descriptor(it, fd)?;
        let t = lumen_os::tty::tcgetattr(fd).map_err(|e| tty_error(it, e))?;
        let (vmin, vtime, icanon) = lumen_os::tty::noncanonical_slots();
        let noncanonical = t.lflag & icanon == 0;
        let cc = t
            .cc
            .iter()
            .enumerate()
            .map(|(i, &c)| if noncanonical && (i == vmin || i == vtime) { Value::Int(c as i64) } else { Value::bytes(vec![c]) })
            .collect();
        Ok(Value::list(vec![
            Value::Int(t.iflag as i64),
            Value::Int(t.oflag as i64),
            Value::Int(t.cflag as i64),
            Value::Int(t.lflag as i64),
            Value::Int(t.ispeed as i64),
            Value::Int(t.ospeed as i64),
            Value::list(cc),
        ]))
    }

    /// Set the tty attributes for file descriptor fd.
    /// The attributes to be set are taken from the attributes argument, which
    /// is a list like the one returned by tcgetattr(). The when argument
    /// determines when the attributes are changed: termios.TCSANOW to
    /// change immediately, termios.TCSADRAIN to change after transmitting all
    /// queued output, or termios.TCSAFLUSH to change after transmitting all
    /// queued output and discarding all queued input.
    #[op]
    fn tcsetattr(it: &mut Interp, fd: &Value, when: i32, attributes: &Value) -> R<()> {
        let fd = as_file_descriptor(it, fd)?;
        let Some(attrs) = items(attributes).filter(|a| a.len() == 7) else {
            return Err(it.type_error("tcsetattr, arg 3: must be 7 element list"));
        };
        lumen_os::tty::tcgetattr(fd).map_err(|e| tty_error(it, e))?;
        let (iflag, oflag, cflag, lflag) = (word(it, &attrs[0])?, word(it, &attrs[1])?, word(it, &attrs[2])?, word(it, &attrs[3])?);
        let (ispeed, ospeed) = (word(it, &attrs[4])?, word(it, &attrs[5])?);
        let nccs = lumen_os::tty::nccs();
        let Some(chars) = items(&attrs[6]).filter(|c| c.len() == nccs) else {
            return Err(it.type_error(&format!("tcsetattr: attributes[6] must be {nccs} element list")));
        };
        let mut cc = Vec::with_capacity(nccs);
        for v in &chars {
            if let Some(b) = bytes_of(v).filter(|b| b.len() == 1) {
                cc.push(b[0]);
            } else if v.is_int_like() {
                cc.push(it.index_of(v)? as u8);
            } else {
                return Err(it.type_error("tcsetattr: elements of attributes must be characters or integers"));
            }
        }
        let mode = Termios { iflag, oflag, cflag, lflag, ispeed, ospeed, cc };
        lumen_os::tty::tcsetattr(fd, when, &mode).map_err(|e| tty_error(it, e))
    }

    /// Send a break on file descriptor fd.
    /// A zero duration sends a break for 0.25-0.5 seconds; a nonzero duration
    /// has a system dependent meaning.
    #[op]
    fn tcsendbreak(it: &mut Interp, fd: &Value, duration: i32) -> R<()> {
        let fd = as_file_descriptor(it, fd)?;
        lumen_os::tty::tcsendbreak(fd, duration).map_err(|e| tty_error(it, e))
    }

    /// Wait until all output written to file descriptor fd has been transmitted.
    #[op]
    fn tcdrain(it: &mut Interp, fd: &Value) -> R<()> {
        let fd = as_file_descriptor(it, fd)?;
        lumen_os::tty::tcdrain(fd).map_err(|e| tty_error(it, e))
    }

    /// Discard queued data on file descriptor fd.
    /// The queue selector specifies which queue: termios.TCIFLUSH for the input
    /// queue, termios.TCOFLUSH for the output queue, or termios.TCIOFLUSH for
    /// both queues.
    #[op]
    fn tcflush(it: &mut Interp, fd: &Value, queue: i32) -> R<()> {
        let fd = as_file_descriptor(it, fd)?;
        lumen_os::tty::tcflush(fd, queue).map_err(|e| tty_error(it, e))
    }

    /// Suspend or resume input or output on file descriptor fd.
    /// The action argument can be termios.TCOOFF to suspend output,
    /// termios.TCOON to restart output, termios.TCIOFF to suspend input,
    /// or termios.TCION to restart input.
    #[op]
    fn tcflow(it: &mut Interp, fd: &Value, action: i32) -> R<()> {
        let fd = as_file_descriptor(it, fd)?;
        lumen_os::tty::tcflow(fd, action).map_err(|e| tty_error(it, e))
    }

    /// Get the tty winsize for file descriptor fd.
    /// Returns a tuple (ws_row, ws_col).
    #[op]
    fn tcgetwinsize(it: &mut Interp, fd: &Value) -> R<Value> {
        let fd = as_file_descriptor(it, fd)?;
        let (rows, cols) = lumen_os::tty::get_winsize(fd).map_err(|e| tty_error(it, e))?;
        Ok(Value::tuple(vec![Value::Int(rows as i64), Value::Int(cols as i64)]))
    }

    /// Set the tty winsize for file descriptor fd.
    /// The winsize to be set is taken from the winsize argument, which
    /// is a two-item tuple (ws_row, ws_col) like the one returned by tcgetwinsize().
    #[op]
    fn tcsetwinsize(it: &mut Interp, fd: &Value, winsize: &Value) -> R<()> {
        let fd = as_file_descriptor(it, fd)?;
        let two = match winsize.tuple_items() {
            Some(t) => Some(t.to_vec()),
            None => items(winsize),
        };
        let Some([rows, cols]) = two.as_deref() else {
            return Err(it.type_error("tcsetwinsize, arg 2: must be a two-item sequence"));
        };
        let (rows, cols) = (it.index_of(rows)?, it.index_of(cols)?);
        lumen_os::tty::get_winsize(fd).map_err(|e| tty_error(it, e))?;
        let (Ok(rows), Ok(cols)) = (u16::try_from(rows), u16::try_from(cols)) else {
            return Err(it.new_exc_str("OverflowError", "winsize value(s) out of range."));
        };
        lumen_os::tty::set_winsize(fd, rows, cols).map_err(|e| tty_error(it, e))
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let exc = it.exc_type("Exception");
        let error = crate::builtins::native::new_type(it, "termios", "error", Some(&exc), Layout::Exception);
        dict_set_str(&d, "error", Value::Obj(error.clone()));
        it.native_state::<State>().error = Some(error);
        for (name, v) in lumen_os::tty::constants() {
            dict_set_str(&d, name, Value::Int(v));
        }
    }
}
