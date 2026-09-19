use std::io::Write;
use std::process::Output;

/// Output policy. Stdout carries only machine-readable results: a path, a
/// list, JSON. Everything a person reads goes to stderr, so
/// `cd "$(wtm new x)"` gets a path and nothing else.
///
/// `emit` is the only write to stdout in the program.
pub struct Ui {
    quiet: bool,
}

impl Ui {
    pub fn new(quiet: bool) -> Ui {
        Ui { quiet }
    }

    pub fn quiet(&self) -> bool {
        self.quiet
    }

    pub fn emit(&self, line: impl AsRef<str>) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", line.as_ref());
        let _ = out.flush();
    }

    pub fn progress(&self, message: impl AsRef<str>) {
        if !self.quiet {
            self.write(message.as_ref());
        }
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        self.write(&format!("warning: {}", message.as_ref()));
    }

    /// Passes on a git subprocess's stderr, where git writes its progress.
    /// Git already words it for a reader, so it goes through unprefixed.
    pub fn relay(&self, output: &Output) {
        let text = String::from_utf8_lossy(&output.stderr);
        let text = text.trim_end();
        if !text.is_empty() && !self.quiet {
            self.write(text);
        }
    }

    fn write(&self, message: &str) {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{message}");
    }
}
