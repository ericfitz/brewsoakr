use std::fmt;

#[derive(Debug)]
pub enum Error {
    Usage(String),
    Refusal(String),
    Brew { status: i32, message: String },
    Io(std::io::Error),
    Other(String),
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) => 2,
            Error::Refusal(_) => 1,
            Error::Brew { status, .. } => *status,
            Error::Io(_) | Error::Other(_) => 1,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(s) | Error::Refusal(s) | Error::Other(s) => write!(f, "{s}"),
            Error::Brew { message, .. } => write!(f, "{message}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_2() {
        assert_eq!(Error::Usage("x".into()).exit_code(), 2);
    }

    #[test]
    fn refusal_is_1() {
        assert_eq!(Error::Refusal("x".into()).exit_code(), 1);
    }

    #[test]
    fn brew_gt_1_is_brew() {
        assert_eq!(
            Error::Brew {
                status: 3,
                message: "x".into()
            }
            .exit_code(),
            3
        );
    }

    #[test]
    fn brew_1_is_1() {
        assert_eq!(
            Error::Brew {
                status: 1,
                message: "x".into()
            }
            .exit_code(),
            1
        );
    }
}
