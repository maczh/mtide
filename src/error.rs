use thiserror::Error;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("plugin error: {0}")]
    Plugin(String),
    #[error("LSP error: {0}")]
    Lsp(String),
    #[error("Git error: {0}")]
    Git(String),
    #[error("workspace error: {0}")]
    Workspace(String),
    #[error("terminal error: {0}")]
    Terminal(String),
}
