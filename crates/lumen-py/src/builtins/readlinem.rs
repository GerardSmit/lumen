//! `readline`: history, completer and hook configuration on `lumen_common::history`. There is no
//! interactive line editor behind it, so `input()` is not routed through it and the editing
//! entry points (`parse_and_bind`, `redisplay`, ...) only keep their state.

/// Importing this module enables command line editing using GNU readline.
#[lumen_bind::module(name = "readline")]
pub mod readline {
    use crate::bind::fspath;
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::history::{truncate_file_text, History};
    use lumen_os::fs::flags::{O_APPEND, O_CREAT, O_TRUNC, O_WRONLY};

    const DEFAULT_DELIMS: &str = " \t\n`~!@#$%^&*()-=+[{]}\\|;:'\",<>/?";

    #[allow(dead_code)]
    pub struct State {
        history: History,
        auto_history: bool,
        line_buffer: String,
        completer: Option<Value>,
        startup_hook: Option<Value>,
        pre_input_hook: Option<Value>,
        display_hook: Option<Value>,
        delims: String,
    }

    impl Default for State {
        fn default() -> State {
            State {
                history: History::new(),
                auto_history: true,
                line_buffer: String::new(),
                completer: None,
                startup_hook: None,
                pre_input_hook: None,
                display_hook: None,
                delims: DEFAULT_DELIMS.to_string(),
            }
        }
    }

    fn hook_slot(v: Option<&Value>) -> Option<Value> {
        v.filter(|v| !matches!(v, Value::None)).cloned()
    }

    fn hook_check(it: &mut Interp, v: Option<&Value>) -> R<()> {
        if let Some(v) = v.filter(|v| !matches!(v, Value::None)) {
            if !it.is_callable(v) {
                return Err(it.type_error("set_hook(function): argument must be callable"));
            }
        }
        Ok(())
    }

    fn file_arg(it: &mut Interp, filename: Option<&Value>) -> R<String> {
        match filename.filter(|v| !matches!(v, Value::None)) {
            Some(v) => {
                let p = fspath(it, v)?;
                match p.as_str() {
                    Some(s) => Ok(s.to_string()),
                    None => Ok(String::from_utf8_lossy(&it.bytes_of(&p)?).into_owned()),
                }
            }
            None => {
                let home = it
                    .platform
                    .borrow()
                    .environ()
                    .into_iter()
                    .find(|(k, _)| k == b"HOME")
                    .map(|(_, v)| String::from_utf8_lossy(&v).into_owned())
                    .unwrap_or_default();
                Ok(format!("{home}/.history"))
            }
        }
    }

    fn os_err(it: &mut Interp, e: lumen_os::FsError) -> Obj {
        it.os_error_errno(e.errno(), None, None)
    }

    fn index_arg(it: &mut Interp, pos: i64, what: &str) -> R<usize> {
        if pos < 0 {
            return Err(it.value_error("History index cannot be negative"));
        }
        let _ = what;
        Ok(pos as usize)
    }

    /// Parse and execute single line of a readline init file.
    #[op]
    fn parse_and_bind(string: &str) {
        let _ = string;
    }

    /// Execute a readline initialization file.
    ///
    /// The default filename is the last filename used.
    #[op]
    fn read_init_file(it: &mut Interp, filename: Option<&Value>) -> R<()> {
        if let Some(v) = filename.filter(|v| !matches!(v, Value::None)) {
            let path = file_arg(it, Some(v))?;
            lumen_os::fs::read_file(&path, 0).map_err(|e| os_err(it, e))?;
        }
        Ok(())
    }

    /// Return the current contents of the line buffer.
    #[op]
    fn get_line_buffer(it: &mut Interp) -> String {
        it.native_state::<State>().line_buffer.clone()
    }

    /// Insert text into the line buffer at the cursor position.
    #[op]
    fn insert_text(it: &mut Interp, string: &str) {
        it.native_state::<State>().line_buffer.push_str(string);
    }

    /// Change what's displayed on the screen to reflect contents of the line buffer.
    #[op]
    fn redisplay() {}

    /// Load a readline history file.
    ///
    /// The default filename is ~/.history.
    #[op]
    fn read_history_file(it: &mut Interp, filename: Option<&Value>) -> R<()> {
        let path = file_arg(it, filename)?;
        let data = lumen_os::fs::read_file(&path, 0).map_err(|e| os_err(it, e))?;
        let text = String::from_utf8_lossy(&data).into_owned();
        it.native_state::<State>().history.load(&text);
        Ok(())
    }

    /// Save a readline history file.
    ///
    /// The default filename is ~/.history.
    #[op]
    fn write_history_file(it: &mut Interp, filename: Option<&Value>) -> R<()> {
        let path = file_arg(it, filename)?;
        let state = it.native_state::<State>();
        let text = state.history.serialize(None);
        let text = truncate_file_text(&text, state.history.max_length());
        lumen_os::fs::write_file(&path, text.as_bytes(), O_WRONLY | O_CREAT | O_TRUNC, 0o666).map_err(|e| os_err(it, e))
    }

    /// Append the last nelements items of the history list to file.
    ///
    /// The default filename is ~/.history.
    #[op]
    fn append_history_file(it: &mut Interp, nelements: i32, filename: Option<&Value>) -> R<()> {
        let path = file_arg(it, filename)?;
        let state = it.native_state::<State>();
        let text = state.history.serialize(Some(nelements.max(0) as usize));
        let max = state.history.max_length();
        lumen_os::fs::write_file(&path, text.as_bytes(), O_WRONLY | O_CREAT | O_APPEND, 0o666).map_err(|e| os_err(it, e))?;
        if max >= 0 {
            let data = lumen_os::fs::read_file(&path, 0).map_err(|e| os_err(it, e))?;
            let kept = truncate_file_text(&String::from_utf8_lossy(&data), max);
            lumen_os::fs::write_file(&path, kept.as_bytes(), O_WRONLY | O_CREAT | O_TRUNC, 0o666).map_err(|e| os_err(it, e))?;
        }
        Ok(())
    }

    /// Set the maximal number of lines which will be written to the history file.
    ///
    /// A negative length is used to inhibit history truncation.
    #[op]
    fn set_history_length(it: &mut Interp, length: i32) {
        it.native_state::<State>().history.set_max_length(length as i64);
    }

    /// Return the maximum number of lines that will be written to the history file.
    #[op]
    fn get_history_length(it: &mut Interp) -> i64 {
        it.native_state::<State>().history.max_length()
    }

    /// Set or remove the function invoked by the rl_startup_hook callback.
    ///
    /// The function is called with no arguments just before readline prints the
    /// first prompt.
    #[op]
    fn set_startup_hook(it: &mut Interp, function: Option<&Value>) -> R<()> {
        hook_check(it, function)?;
        it.native_state::<State>().startup_hook = hook_slot(function);
        Ok(())
    }

    /// Set or remove the function invoked by the rl_pre_input_hook callback.
    ///
    /// The function is called with no arguments after the first prompt
    /// has been printed and just before readline starts reading input
    /// characters.
    #[op]
    fn set_pre_input_hook(it: &mut Interp, function: Option<&Value>) -> R<()> {
        hook_check(it, function)?;
        it.native_state::<State>().pre_input_hook = hook_slot(function);
        Ok(())
    }

    /// Set or remove the completion display function.
    ///
    /// The function is called as
    ///   function(substitution, [matches], longest_match_length)
    /// once each time matches need to be displayed.
    #[op]
    fn set_completion_display_matches_hook(it: &mut Interp, function: Option<&Value>) -> R<()> {
        hook_check(it, function)?;
        it.native_state::<State>().display_hook = hook_slot(function);
        Ok(())
    }

    /// Set or remove the completer function.
    ///
    /// The function is called as function(text, state),
    /// for state in 0, 1, 2, ..., until it returns a non-string.
    /// It should return the next possible completion starting with 'text'.
    #[op]
    fn set_completer(it: &mut Interp, function: Option<&Value>) -> R<()> {
        hook_check(it, function)?;
        it.native_state::<State>().completer = hook_slot(function);
        Ok(())
    }

    /// Get the current completer function.
    #[op]
    fn get_completer(it: &mut Interp) -> Value {
        it.native_state::<State>().completer.clone().unwrap_or(Value::None)
    }

    /// Get the type of completion being attempted.
    #[op]
    fn get_completion_type() -> i32 {
        0
    }

    /// Get the beginning index of the completion scope.
    #[op]
    fn get_begidx() -> i32 {
        0
    }

    /// Get the ending index of the completion scope.
    #[op]
    fn get_endidx() -> i32 {
        0
    }

    /// Set the word delimiters for completion.
    #[op]
    fn set_completer_delims(it: &mut Interp, string: &str) {
        it.native_state::<State>().delims = string.to_string();
    }

    /// Get the word delimiters for completion.
    #[op]
    fn get_completer_delims(it: &mut Interp) -> String {
        it.native_state::<State>().delims.clone()
    }

    /// Clear the current readline history.
    #[op]
    fn clear_history(it: &mut Interp) {
        it.native_state::<State>().history.clear();
    }

    /// Return the current number of items in the history.
    #[op]
    fn get_current_history_length(it: &mut Interp) -> i64 {
        it.native_state::<State>().history.len() as i64
    }

    /// Return the current contents of history item at one-based index.
    #[op]
    fn get_history_item(it: &mut Interp, index: i64) -> Value {
        it.native_state::<State>().history.get(index).map_or(Value::None, Value::str)
    }

    /// Remove history item given by its zero-based position.
    #[op]
    fn remove_history_item(it: &mut Interp, pos: i64) -> R<()> {
        let n = index_arg(it, pos, "remove")?;
        if it.native_state::<State>().history.remove(n).is_none() {
            return Err(it.value_error(&format!("No history item at position {pos}")));
        }
        Ok(())
    }

    /// Replaces history item given by its position with contents of line.
    ///
    ///   pos
    ///     zero-based index in the history list
    ///   line
    ///     line to replace with
    #[op]
    fn replace_history_item(it: &mut Interp, pos: i64, line: &str) -> R<()> {
        let n = index_arg(it, pos, "replace")?;
        if !it.native_state::<State>().history.replace(n, line) {
            return Err(it.value_error(&format!("No history item at position {pos}")));
        }
        Ok(())
    }

    /// Add an item to the history buffer.
    #[op]
    fn add_history(it: &mut Interp, string: &str) {
        it.native_state::<State>().history.add(string);
    }

    /// Enables or disables automatic history.
    #[op]
    fn set_auto_history(it: &mut Interp, enabled: bool) {
        it.native_state::<State>().auto_history = enabled;
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        dict_set_str(&d, "_READLINE_VERSION", Value::Int(0x0802));
        dict_set_str(&d, "_READLINE_RUNTIME_VERSION", Value::Int(0x0802));
        dict_set_str(&d, "_READLINE_LIBRARY_VERSION", Value::str("8.2"));
    }
}
