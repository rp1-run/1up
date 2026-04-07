use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::shared::config;
use crate::shared::errors::{DaemonError, OneupError};
use crate::shared::types::SearchResult;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SearchRequest {
    pub project_root: PathBuf,
    pub query: String,
    pub limit: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum SearchResponse {
    Results { results: Vec<SearchResult> },
    Unavailable { reason: String },
}

pub(crate) async fn bind_listener() -> Result<UnixListener, OneupError> {
    bind_listener_at(&config::daemon_socket_path()?).await
}

pub(crate) fn cleanup_socket_file() -> Result<(), OneupError> {
    cleanup_socket_file_at(&config::daemon_socket_path()?)
}

pub(crate) async fn accept_request(
    listener: &UnixListener,
) -> Result<(UnixStream, SearchRequest), OneupError> {
    let (mut stream, _) = listener.accept().await.map_err(|err| {
        DaemonError::RequestError(format!("failed to accept search request: {err}"))
    })?;
    let request = read_message(&mut stream).await?;
    Ok((stream, request))
}

pub(crate) async fn send_response(
    stream: &mut UnixStream,
    response: &SearchResponse,
) -> Result<(), OneupError> {
    write_message(stream, response).await
}

pub(crate) async fn request_search(
    project_root: &Path,
    query: &str,
    limit: usize,
) -> Result<Option<Vec<SearchResult>>, OneupError> {
    request_search_at(&config::daemon_socket_path()?, project_root, query, limit).await
}

async fn bind_listener_at(socket_path: &Path) -> Result<UnixListener, OneupError> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            DaemonError::RequestError(format!(
                "failed to create search socket directory {}: {err}",
                parent.display()
            ))
        })?;
    }

    cleanup_socket_file_at(socket_path)?;

    UnixListener::bind(socket_path).map_err(|err| {
        DaemonError::RequestError(format!(
            "failed to bind search socket {}: {err}",
            socket_path.display()
        ))
        .into()
    })
}

fn cleanup_socket_file_at(socket_path: &Path) -> Result<(), OneupError> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path).map_err(|err| {
            DaemonError::RequestError(format!(
                "failed to remove search socket {}: {err}",
                socket_path.display()
            ))
        })?;
    }

    Ok(())
}

async fn request_search_at(
    socket_path: &Path,
    project_root: &Path,
    query: &str,
    limit: usize,
) -> Result<Option<Vec<SearchResult>>, OneupError> {
    let mut stream = UnixStream::connect(socket_path).await.map_err(|err| {
        DaemonError::RequestError(format!(
            "failed to connect to search socket {}: {err}",
            socket_path.display()
        ))
    })?;

    let request = SearchRequest {
        project_root: project_root.to_path_buf(),
        query: query.to_string(),
        limit,
    };
    write_message(&mut stream, &request).await?;

    match read_message(&mut stream).await? {
        SearchResponse::Results { results } => Ok(Some(results)),
        SearchResponse::Unavailable { .. } => Ok(None),
    }
}

async fn read_message<T>(stream: &mut UnixStream) -> Result<T, OneupError>
where
    T: DeserializeOwned,
{
    let mut payload = Vec::new();
    stream.read_to_end(&mut payload).await.map_err(|err| {
        DaemonError::RequestError(format!("failed to read search socket payload: {err}"))
    })?;
    serde_json::from_slice(&payload).map_err(|err| {
        DaemonError::RequestError(format!("failed to decode search socket payload: {err}")).into()
    })
}

async fn write_message<T>(stream: &mut UnixStream, value: &T) -> Result<(), OneupError>
where
    T: Serialize,
{
    let payload = serde_json::to_vec(value).map_err(|err| {
        DaemonError::RequestError(format!("failed to encode search socket payload: {err}"))
    })?;
    stream.write_all(&payload).await.map_err(|err| {
        DaemonError::RequestError(format!("failed to write search socket payload: {err}"))
    })?;
    stream.shutdown().await.map_err(|err| {
        DaemonError::RequestError(format!("failed to close search socket writer: {err}"))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_search_returns_results() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("daemon.sock");
        let listener = bind_listener_at(&socket_path).await.unwrap();
        let project_root = tmp.path().join("project");

        let expected_root = project_root.clone();
        let server = tokio::spawn(async move {
            let (mut stream, request) = accept_request(&listener).await.unwrap();
            assert_eq!(
                request,
                SearchRequest {
                    project_root: expected_root,
                    query: "needle".to_string(),
                    limit: 7,
                }
            );
            send_response(
                &mut stream,
                &SearchResponse::Results {
                    results: vec![SearchResult {
                        file_path: "src/lib.rs".to_string(),
                        language: "rust".to_string(),
                        block_type: "function".to_string(),
                        content: "fn needle() {}".to_string(),
                        score: 1.0,
                        line_number: 1,
                        line_end: 1,
                        breadcrumb: None,
                        complexity: None,
                        role: None,
                        defined_symbols: None,
                        referenced_symbols: None,
                        called_symbols: None,
                    }],
                },
            )
            .await
            .unwrap();
        });

        let results = request_search_at(&socket_path, &project_root, "needle", 7)
            .await
            .unwrap()
            .unwrap();

        server.await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_path, "src/lib.rs");
    }

    #[tokio::test]
    async fn request_search_returns_none_for_unavailable_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("daemon.sock");
        let listener = bind_listener_at(&socket_path).await.unwrap();
        let project_root = tmp.path().join("project");

        let server = tokio::spawn(async move {
            let (mut stream, _) = accept_request(&listener).await.unwrap();
            send_response(
                &mut stream,
                &SearchResponse::Unavailable {
                    reason: "project not registered".to_string(),
                },
            )
            .await
            .unwrap();
        });

        let results = request_search_at(&socket_path, &project_root, "needle", 7)
            .await
            .unwrap();

        server.await.unwrap();
        assert!(results.is_none());
    }
}
