#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Date parse error: {0}")]
    DateParse(String),
    #[error("{0}")]
    General(String),
}

pub type Result<T> = std::result::Result<T, AppError>;
