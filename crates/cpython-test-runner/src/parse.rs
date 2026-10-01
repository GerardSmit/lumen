//! Parsing of `unittest -v` output into per-file counts.

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counts {
    pub pass: u32,
    pub fail: u32,
    pub error: u32,
    pub skip: u32,
}

impl Counts {
    pub fn total(&self) -> u32 {
        self.pass + self.fail + self.error + self.skip
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Every test that ran passed (skips allowed) and at least one ran.
    Ok,
    /// At least one test failed or errored.
    Failed,
    /// The file produced no test results; the reason is the last line of output.
    ImportError(String),
    Timeout,
    Crash(String),
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Failed => "failed",
            Status::ImportError(_) => "import-error",
            Status::Timeout => "timeout",
            Status::Crash(_) => "crash",
        }
    }
}

pub fn parse_counts(output: &str) -> Counts {
    let mut c = Counts::default();
    for line in output.lines() {
        let line = line.trim_end();
        let Some((_, verdict)) = line.rsplit_once(" ... ") else { continue };
        let verdict = verdict.trim();
        if verdict == "ok" || verdict == "expected failure" {
            c.pass += 1;
        } else if verdict == "FAIL" || verdict == "unexpected success" {
            c.fail += 1;
        } else if verdict == "ERROR" {
            c.error += 1;
        } else if verdict.starts_with("skipped") {
            c.skip += 1;
        }
    }
    c
}

pub fn last_line(output: &str) -> String {
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output")
        .chars()
        .take(200)
        .collect()
}

pub fn classify(counts: Counts, exit_code: Option<i32>, output: &str) -> Status {
    if counts.total() == 0 {
        return match exit_code {
            Some(0) if output.contains("Ran 0 tests") => Status::ImportError("ran 0 tests".to_string()),
            Some(_) => Status::ImportError(last_line(output)),
            None => Status::Crash(last_line(output)),
        };
    }
    if counts.fail + counts.error > 0 {
        Status::Failed
    } else if exit_code.is_none() {
        Status::Crash(last_line(output))
    } else {
        Status::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
test_a (test_x.T.test_a) ... ok
test_b (test_x.T.test_b) ... FAIL
test_c (test_x.T.test_c)
Docstring line ... ERROR
test_d (test_x.T.test_d) ... skipped 'no network'
test_e (test_x.T.test_e) ... expected failure
test_f (test_x.T.test_f) ... unexpected success

Ran 6 tests in 0.1s
";

    #[test]
    fn counts_every_verdict() {
        let c = parse_counts(SAMPLE);
        assert_eq!(c, Counts { pass: 2, fail: 2, error: 1, skip: 1 });
    }

    #[test]
    fn classifies_import_errors() {
        let out = "Traceback (most recent call last):\nModuleNotFoundError: No module named '_io'\n";
        let s = classify(parse_counts(out), Some(1), out);
        assert_eq!(s, Status::ImportError("ModuleNotFoundError: No module named '_io'".to_string()));
    }

    #[test]
    fn classifies_ok_and_failed() {
        assert_eq!(classify(Counts { pass: 3, ..Counts::default() }, Some(0), ""), Status::Ok);
        assert_eq!(classify(Counts { pass: 3, fail: 1, ..Counts::default() }, Some(1), ""), Status::Failed);
    }

    #[test]
    fn signal_exit_without_results_is_a_crash() {
        assert!(matches!(classify(Counts::default(), None, "boom"), Status::Crash(_)));
    }
}
