//! Uncaught-exception reporting: CPython-format tracebacks with cause/context chains.

use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

impl Interp {
    pub fn source_line(&mut self, file: &str, line: u32) -> Option<String> {
        if !self.sources.contains_key(file) {
            let read = self.platform.borrow_mut().read_file(file);
            let lines = read
                .map(|b| {
                    String::from_utf8_lossy(&b)
                        .lines()
                        .map(|l| l.to_string())
                        .collect()
                })
                .unwrap_or_default();
            self.sources.insert(file.to_string(), lines);
        }
        let l = self
            .sources
            .get(file)?
            .get((line as usize).checked_sub(1)?)?;
        let t = l.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    }

    pub fn exc_header(&mut self, exc: &Obj) -> String {
        let cls = self.type_of_obj(exc);
        let name = self.type_qualname(&cls);
        let name = match self.type_module(&cls) {
            Some(m) if m != "builtins" && m != "__main__" => format!("{}.{}", m, name),
            _ => name,
        };
        // A syntax error shows its location on the lines above, so only its `msg` here.
        let msg_attr = match self.is_exc_instance(exc, "SyntaxError") {
            true => exc
                .dict
                .borrow()
                .as_ref()
                .and_then(|d| dict_get_str(d, "msg"))
                .filter(|m| !m.is_none()),
            false => None,
        };
        let shown = msg_attr.unwrap_or_else(|| Value::Obj(exc.clone()));
        let msg = match self.str_of(&shown) {
            Ok(m) => m,
            Err(_) => "<exception str() failed>".to_string(),
        };
        if msg.is_empty() {
            name
        } else {
            format!("{}: {}", name, msg)
        }
    }

    pub fn format_exception(&mut self, exc: &Obj) -> String {
        let mut seen: Vec<*const Object> = Vec::new();
        let mut out = String::new();
        self.format_chain(exc, &mut seen, &mut out);
        out
    }

    fn format_chain(&mut self, exc: &Obj, seen: &mut Vec<*const Object>, out: &mut String) {
        seen.push(Rc::as_ptr(exc));
        let (cause, context, suppress, tb) = match &exc.kind {
            Kind::Exception(d) => {
                let d = d.borrow();
                let tb: Vec<(Rc<str>, u32, Rc<str>)> =
                    d.tb.iter()
                        .map(|t| (t.file.clone(), t.line, t.name.clone()))
                        .collect();
                (d.cause.clone(), d.context.clone(), d.suppress_context, tb)
            }
            _ => (None, None, false, Vec::new()),
        };
        if let Some(c) = &cause {
            if !seen.contains(&Rc::as_ptr(c)) {
                self.format_chain(c, seen, out);
                out.push_str(
                    "\nThe above exception was the direct cause of the following exception:\n\n",
                );
            }
        } else if let Some(c) = &context {
            if !suppress && !seen.contains(&Rc::as_ptr(c)) {
                self.format_chain(c, seen, out);
                out.push_str(
                    "\nDuring handling of the above exception, another exception occurred:\n\n",
                );
            }
        }
        if self.is_exc_instance(exc, "BaseExceptionGroup") {
            self.format_group(exc, 2, true, out);
            return;
        }
        if !tb.is_empty() {
            out.push_str("Traceback (most recent call last):\n");
            for (file, line, name) in tb.iter().rev() {
                out.push_str(&format!(
                    "  File \"{}\", line {}, in {}\n",
                    file, line, name
                ));
                if let Some(src) = self.source_line(file, *line) {
                    out.push_str(&format!("    {}\n", src));
                }
            }
        }
        if self.is_exc_instance(exc, "SyntaxError") {
            if let Some(s) = self.syntax_error_detail(exc) {
                out.push_str(&s);
            }
        }
        out.push_str(&self.exc_header_with_notes(exc));
        out.push('\n');
    }

    fn exc_header_with_notes(&mut self, exc: &Obj) -> String {
        let mut s = self.exc_header(exc);
        let notes = exc
            .dict
            .borrow()
            .as_ref()
            .and_then(|d| dict_get_str(d, "__notes__"));
        if let Some(Value::Obj(l)) = notes {
            if let Kind::List(l) = &l.kind {
                let items = l.borrow().clone();
                for n in items {
                    let t = self.str_of(&n).unwrap_or_default();
                    s.push('\n');
                    s.push_str(&t);
                }
            }
        }
        s
    }

    /// Renders an exception group in CPython's boxed layout; `indent` is the column of the bar.
    fn format_group(&mut self, exc: &Obj, indent: usize, top: bool, out: &mut String) {
        let pad = " ".repeat(indent);
        let tb: Vec<(Rc<str>, u32, Rc<str>)> = match &exc.kind {
            Kind::Exception(d) => d
                .borrow()
                .tb
                .iter()
                .map(|t| (t.file.clone(), t.line, t.name.clone()))
                .collect(),
            _ => Vec::new(),
        };
        let mut lines: Vec<String> = Vec::new();
        if !tb.is_empty() {
            lines.push("Exception Group Traceback (most recent call last):".to_string());
            for (file, line, name) in tb.iter().rev() {
                lines.push(format!("  File \"{}\", line {}, in {}", file, line, name));
                if let Some(src) = self.source_line(file, *line) {
                    lines.push(format!("    {}", src));
                }
            }
        }
        lines.extend(
            self.exc_header_with_notes(exc)
                .split('\n')
                .map(str::to_string),
        );
        for (i, l) in lines.iter().enumerate() {
            if i == 0 && top && !tb.is_empty() {
                out.push_str(&format!("{}+ {}\n", pad, l));
            } else {
                out.push_str(&format!("{}| {}\n", pad, l));
            }
        }
        let subs = crate::builtins::excgroup::group_items(exc);
        let n = subs.len();
        for (i, sub) in subs.iter().enumerate() {
            let sep = format!("{} {} {}", "-".repeat(16), i + 1, "-".repeat(16));
            if i == 0 {
                out.push_str(&format!("{}+-+{}\n", pad, sep));
            } else {
                out.push_str(&format!("{}  +{}\n", pad, sep));
            }
            if let Value::Obj(so) = sub {
                if self.is_exc_instance(so, "BaseExceptionGroup") {
                    self.format_group(so, indent + 2, false, out);
                } else {
                    let tbs: Vec<(Rc<str>, u32, Rc<str>)> = match &so.kind {
                        Kind::Exception(d) => d
                            .borrow()
                            .tb
                            .iter()
                            .map(|t| (t.file.clone(), t.line, t.name.clone()))
                            .collect(),
                        _ => Vec::new(),
                    };
                    let mut sl: Vec<String> = Vec::new();
                    if !tbs.is_empty() {
                        sl.push("Traceback (most recent call last):".to_string());
                        for (file, line, name) in tbs.iter().rev() {
                            sl.push(format!("  File \"{}\", line {}, in {}", file, line, name));
                            if let Some(src) = self.source_line(file, *line) {
                                sl.push(format!("    {}", src));
                            }
                        }
                    }
                    sl.extend(
                        self.exc_header_with_notes(so)
                            .split('\n')
                            .map(str::to_string),
                    );
                    for l in sl {
                        out.push_str(&format!("{}  | {}\n", pad, l));
                    }
                }
            }
            let sub_is_group =
                matches!(sub, Value::Obj(so) if self.is_exc_instance(so, "BaseExceptionGroup"));
            if i + 1 == n && !sub_is_group {
                out.push_str(&format!("{}  +{}\n", pad, "-".repeat(36)));
            }
        }
    }

    pub(crate) fn is_exc_instance(&self, e: &Obj, name: &str) -> bool {
        let c = self.type_of_obj(e);
        self.is_subtype(&c, &self.exc_type(name))
    }

    fn syntax_error_detail(&mut self, exc: &Obj) -> Option<String> {
        let d = exc.dict.borrow().clone()?;
        let file = dict_get_str(&d, "filename")?;
        let line = dict_get_str(&d, "lineno")?;
        let mut s = format!(
            "  File \"{}\", line {}\n",
            file.as_str().unwrap_or("<unknown>"),
            line.as_i64().unwrap_or(0)
        );
        if let (Some(f), Some(l)) = (file.as_str(), line.as_i64()) {
            if let Some(src) = self.source_line(f, l as u32) {
                s.push_str(&format!("    {}\n", src));
            }
        }
        Some(s)
    }

    pub fn print_exception(&mut self, exc: &Obj) {
        let s = self.format_exception(exc);
        self.write_stderr(&s);
    }

    /// Reports an uncaught exception through `sys.excepthook` when a script replaced it.
    fn call_excepthook(&mut self, exc: &Obj) {
        let d = self.sys_module.clone().map(|m| self.module_dict(&m));
        let hook = d.as_ref().and_then(|d| dict_get_str(d, "excepthook"));
        let original = d.as_ref().and_then(|d| dict_get_str(d, "__excepthook__"));
        let hook = match (hook, original) {
            (Some(Value::Obj(h)), Some(Value::Obj(o))) if !Rc::ptr_eq(&h, &o) => Value::Obj(h),
            _ => return self.print_exception(exc),
        };
        let t = Value::Obj(self.type_of_obj(exc));
        let tb = match &exc.kind {
            Kind::Exception(d) => self.make_tb(&d.borrow().tb),
            _ => Value::None,
        };
        if let Err(e) = self.call(&hook, vec![t, Value::Obj(exc.clone()), tb], Vec::new()) {
            self.write_stderr("Error in sys.excepthook:\n");
            self.print_exception(&e);
            self.write_stderr("\nOriginal exception was:\n");
            self.print_exception(exc);
        }
    }

    /// Exit status for an exception that reached the top level (printing it unless `SystemExit`).
    pub fn report_uncaught(&mut self, exc: &Obj) -> i32 {
        if self.is_exc_instance(exc, "SystemExit") {
            let args = match &exc.kind {
                Kind::Exception(d) => d.borrow().args.clone(),
                _ => Value::None,
            };
            let first = args.tuple_items().and_then(|t| t.first().cloned());
            return match first {
                None | Some(Value::None) => 0,
                Some(Value::Int(i)) => i as i32,
                Some(Value::Bool(b)) => b as i32,
                Some(v) => {
                    let s = self.str_of(&v).unwrap_or_default();
                    self.write_stderr(&format!("{}\n", s));
                    1
                }
            };
        }
        self.call_excepthook(exc);
        if self.is_exc_instance(exc, "KeyboardInterrupt") {
            return crate::limits::EXIT_INTERRUPTED;
        }
        1
    }
}
