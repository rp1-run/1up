use crate::daemon::lifecycle;
use crate::shared::errors::{DaemonError, OneupError};

pub async fn run() -> Result<(), OneupError> {
    Err(DaemonError::RequestError(lifecycle::unsupported_message().to_string()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn worker_run_returns_unsupported_error() {
        let err = run().await.unwrap_err();
        assert!(err.to_string().contains("background daemon workflows"));
    }
}
