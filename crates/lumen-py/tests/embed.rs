use lumen_py::{Interp, Output};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Default)]
struct Captured {
    out: Vec<u8>,
    err: Vec<u8>,
}

struct Capture(Rc<RefCell<Captured>>);

impl Output for Capture {
    fn write_stdout(&mut self, bytes: &[u8]) {
        self.0.borrow_mut().out.extend_from_slice(bytes);
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        self.0.borrow_mut().err.extend_from_slice(bytes);
    }
}

fn interp() -> (Interp, Rc<RefCell<Captured>>) {
    let cap = Rc::new(RefCell::new(Captured::default()));
    let mut it = Interp::new();
    it.set_output(Box::new(Capture(cap.clone())));
    (it, cap)
}

fn on_big_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(1 << 26).spawn(f).unwrap().join().unwrap();
}

#[test]
fn output_is_captured_and_exit_status_returned() {
    on_big_stack(|| {
        let (mut it, cap) = interp();
        it.set_argv(&["prog".into(), "x".into()]);
        let code = it.run_source("import sys\nprint('hi', sys.argv)\nsys.stderr.write('warn\\n')\nsys.exit(3)\n", "<embed>");
        it.flush_out();
        assert_eq!(code, 3);
        assert_eq!(String::from_utf8_lossy(&cap.borrow().out), "hi ['prog', 'x']\n");
        assert_eq!(String::from_utf8_lossy(&cap.borrow().err), "warn\n");
    });
}

#[test]
fn uncaught_exception_reports_on_stderr() {
    on_big_stack(|| {
        let (mut it, cap) = interp();
        let code = it.run_source("raise ValueError('boom')\n", "<embed>");
        assert_eq!(code, 1);
        assert!(String::from_utf8_lossy(&cap.borrow().err).contains("ValueError: boom"));
    });
}

#[test]
fn interpreters_do_not_share_state() {
    on_big_stack(|| {
        let (mut a, out_a) = interp();
        let (mut b, out_b) = interp();
        assert_eq!(a.run_source("x = [1, 2]\nx.append(3)\nprint(x)\n", "<a>"), 0);
        assert_eq!(b.run_source("print('x' in dir())\n", "<b>"), 0);
        a.flush_out();
        b.flush_out();
        assert_eq!(String::from_utf8_lossy(&out_a.borrow().out), "[1, 2, 3]\n");
        assert_eq!(String::from_utf8_lossy(&out_b.borrow().out), "False\n");
    });
}
