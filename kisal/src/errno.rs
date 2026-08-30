//! Linux errno values, and how a syscall reports one.
//!
//! A failing syscall returns the negated errno in `rax`; every libc's
//! wrapper recognises the `-4095..=-1` band and moves it into `errno`. So
//! there is no separate error channel to design — the return value *is* the
//! channel, and [`Errno::as_result`] is the whole encoding.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum Errno {
    Perm = 1,
    NoEntry = 2,
    /// No such process. `prlimit64` is the one row that names a process
    /// other than this one, and there is no other one.
    NoProcess = 3,
    Io = 5,
    NoDevice = 19,
    BadFile = 9,
    NoMemory = 12,
    Access = 13,
    Fault = 14,
    /// `EAGAIN`: a futex whose value has already changed, which is the
    /// answer that closes the window between deciding to sleep and sleeping.
    TryAgain = 11,
    Busy = 16,
    CrossDevice = 18,
    Exists = 17,
    NotDir = 20,
    IsDir = 21,
    Invalid = 22,
    FileTableFull = 23,
    TooManyFiles = 24,
    NoTty = 25,
    TooBig = 27,
    NoSpace = 28,
    NotSeekable = 29,
    ReadOnlyFs = 30,
    Pipe = 32,
    Range = 34,
    NoData = 61,
    NameTooLong = 36,
    NoSys = 38,
    NotEmpty = 39,
    Loop = 40,
    NotSupported = 95,
    /// `EAFNOSUPPORT`: the address family is not one this machine has.
    ///
    /// The honest answer for a container with no sockets. See the `socket`
    /// row for why refusing by name would be the wrong loudness.
    AddressFamily = 97,
    /// Not a Linux errno: this kernel cannot reach a file by descriptor
    /// alone in order to change it. Distinct so that the one row that can
    /// produce it turns into a named fault rather than a plausible answer.
    NameNeeded = 1000,
}

impl Errno {
    /// The value a failing syscall leaves in `rax`.
    pub fn as_result(self) -> i64 {
        -(self as i32 as i64)
    }
}

impl Errno {
    /// The C name, for a diagnostic. A number alone in a log line is a
    /// number somebody has to look up.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Perm => "EPERM",
            Self::NoEntry => "ENOENT",
            Self::NoProcess => "ESRCH",
            Self::Io => "EIO",
            Self::NoDevice => "ENODEV",
            Self::BadFile => "EBADF",
            Self::NoMemory => "ENOMEM",
            Self::Access => "EACCES",
            Self::Fault => "EFAULT",
            Self::Busy => "EBUSY",
            Self::CrossDevice => "EXDEV",
            Self::Exists => "EEXIST",
            Self::NotDir => "ENOTDIR",
            Self::IsDir => "EISDIR",
            Self::Invalid => "EINVAL",
            Self::FileTableFull => "ENFILE",
            Self::TooManyFiles => "EMFILE",
            Self::NoTty => "ENOTTY",
            Self::TooBig => "EFBIG",
            Self::NoSpace => "ENOSPC",
            Self::NotSeekable => "ESPIPE",
            Self::ReadOnlyFs => "EROFS",
            Self::Pipe => "EPIPE",
            Self::Range => "ERANGE",
            Self::NoData => "ENODATA",
            Self::NameTooLong => "ENAMETOOLONG",
            Self::NotEmpty => "ENOTEMPTY",
            Self::NoSys => "ENOSYS",
            Self::Loop => "ELOOP",
            Self::NotSupported => "ENOTSUP",
            Self::AddressFamily => "EAFNOSUPPORT",
            Self::TryAgain => "EAGAIN",
            Self::NameNeeded => "a name this kernel did not keep",
        }
    }
}
