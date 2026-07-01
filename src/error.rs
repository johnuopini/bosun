use thiserror::Error;

pub type Result<T> = std::result::Result<T, BosunError>;

#[derive(Debug, Error)]
pub enum BosunError {
    #[error("tmux command failed: {0}")]
    Tmux(String),

    #[error("git command failed: {0}")]
    Git(String),

    #[error("tmux not found on PATH")]
    TmuxNotInstalled,

    #[error("failed to parse tmux output: {0}")]
    Parse(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("store error: {0}")]
    Store(String),

    #[error("channel closed unexpectedly")]
    ChannelClosed,
}
