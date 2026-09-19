use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

/// Every failure `wtm` can report. The exit code lives here and nowhere
/// else, so one place decides the mapping.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("{0}")]
    Usage(String),

    #[error("git {} failed ({status}): {}", args.join(" "), stderr.trim())]
    Git {
        args: Vec<String>,
        status: i32,
        stderr: String,
    },

    #[error("git is {found}, but wtm needs at least {needed}")]
    GitVersion { found: String, needed: String },

    #[error("not inside a git repository: {0}")]
    NotARepo(PathBuf),

    #[error("a {0} is in progress; finish or abort it first")]
    InProgress(&'static str),

    #[error("{0} has uncommitted changes; commit them or pass --force")]
    Dirty(PathBuf),

    #[error("{0} is locked; unlock it with `git worktree unlock` or pass --force")]
    Locked(PathBuf),

    #[error("init hook failed (exit {code}); worktree kept at {}; rerun with: wtm init {name}", worktree.display())]
    HookFailed {
        code: i32,
        worktree: PathBuf,
        name: String,
    },

    #[error("copy-on-write cloning is unavailable: {reason}")]
    CloneUnsupported { reason: String },

    #[error("branch {branch} is already checked out at {}", at.display())]
    BranchCheckedOut { branch: String, at: PathBuf },

    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{}: {message}", file.display())]
    Config { file: PathBuf, message: String },

    #[error("{0}")]
    Index(String),

    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) | Error::NotImplemented(_) => 2,
            Error::HookFailed { .. } => 3,
            Error::Dirty(_) | Error::Locked(_) => 4,
            _ => 1,
        }
    }

    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub fn usage(message: impl Into<String>) -> Error {
        Error::Usage(message.into())
    }
}
