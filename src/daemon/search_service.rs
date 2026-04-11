use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};

use crate::daemon::ipc;
use crate::shared::config;
use crate::shared::constants::{
    DAEMON_READ_TIMEOUT_MS, DAEMON_WRITE_TIMEOUT_MS, MAX_DAEMON_QUERY_BYTES,
    MAX_DAEMON_REQUEST_BYTES, MAX_DAEMON_RESPONSE_BYTES, MAX_SEARCH_RESULTS, SECURE_SOCKET_MODE,
    XDG_STATE_DIR_MODE,
};
use crate::shared::errors::{DaemonError, OneupError};
use crate::shared::fs::{
    ensure_secure_dir_within_root, ensure_secure_xdg_root, remove_socket_file,
};
use crate::shared::types::SearchResult;

const SAFE_UNAVAILABLE_REASON: &str = "daemon unavailable";
const SAFE_BUSY_REASON: &str = "daemon busy";

pub(crate) struct SearchListener {
    listener: UnixListener,
    daemon_uid: u32,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SearchRequest {
    pub project_root: PathBuf,
    pub query: String,
    pub limit: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum SearchResponse {
    Results {
        results: Vec<SearchResult>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        daemon_version: Option<String>,
    },
    Unavailable {
        reason: String,
    },
}

pub(crate) async fn bind_listener() -> Result<SearchListener, OneupError> {
    bind_listener_at(&config::daemon_socket_path()?).await
}

pub(crate) fn cleanup_socket_file() -> Result<(), OneupError> {
    let xdg_root = ensure_secure_xdg_root()?;
    cleanup_socket_file_at(&config::daemon_socket_path()?, &xdg_root)
}

pub(crate) async fn accept_connection(
    listener: &SearchListener,
) -> Result<Option<UnixStream>, OneupError> {
    let (mut stream, _) = listener.listener.accept().await.map_err(|err| {
        DaemonError::RequestError(format!("failed to accept search request: {err}"))
    })?;
    let is_authorized = ipc::authorize_same_uid(&stream, listener.daemon_uid).unwrap_or(false);
    if !is_authorized {
        let _ = send_unavailable_response(&mut stream).await;
        return Ok(None);
    }

    Ok(Some(stream))
}

pub(crate) async fn read_request(stream: &mut UnixStream) -> Result<SearchRequest, OneupError> {
    let request = ipc::read_json_frame(stream, MAX_DAEMON_REQUEST_BYTES, read_deadline()).await?;
    sanitize_request(request)
}

pub(crate) async fn send_response(
    stream: &mut UnixStream,
    response: &SearchResponse,
) -> Result<(), OneupError> {
    ipc::write_json_frame(
        stream,
        response,
        MAX_DAEMON_RESPONSE_BYTES,
        write_deadline(),
    )
    .await
}

pub(crate) async fn request_search(
    project_root: &Path,
    query: &str,
    limit: usize,
) -> Result<Option<(Vec<SearchResult>, Option<String>)>, OneupError> {
    request_search_at(&config::daemon_socket_path()?, project_root, query, limit).await
}

pub(crate) fn unavailable_response() -> SearchResponse {
    SearchResponse::Unavailable {
        reason: SAFE_UNAVAILABLE_REASON.to_string(),
    }
}

pub(crate) fn busy_response() -> SearchResponse {
    SearchResponse::Unavailable {
        reason: SAFE_BUSY_REASON.to_string(),
    }
}

pub(crate) async fn send_unavailable_response(stream: &mut UnixStream) -> Result<(), OneupError> {
    send_response(stream, &unavailable_response()).await
}

pub(crate) async fn send_busy_response(stream: &mut UnixStream) -> Result<(), OneupError> {
    send_response(stream, &busy_response()).await
}

async fn bind_listener_at(socket_path: &Path) -> Result<SearchListener, OneupError> {
    let xdg_root = ensure_secure_xdg_root()?;
    let parent = socket_path.parent().ok_or_else(|| {
        DaemonError::RequestError(format!(
            "search socket path must include a parent directory: {}",
            socket_path.display()
        ))
    })?;
    ensure_secure_dir_within_root(parent, &xdg_root, XDG_STATE_DIR_MODE)?;
    cleanup_socket_file_at(socket_path, &xdg_root)?;

    let listener = UnixListener::bind(socket_path).map_err(|err| -> OneupError {
        DaemonError::RequestError(format!(
            "failed to bind search socket {}: {err}",
            socket_path.display()
        ))
        .into()
    })?;
    std::fs::set_permissions(
        socket_path,
        std::fs::Permissions::from_mode(SECURE_SOCKET_MODE),
    )
    .map_err(|err| {
        DaemonError::RequestError(format!(
            "failed to secure search socket {}: {err}",
            socket_path.display()
        ))
    })?;
    let daemon_uid = std::fs::metadata(socket_path)
        .map_err(|err| {
            DaemonError::RequestError(format!(
                "failed to stat search socket {}: {err}",
                socket_path.display()
            ))
        })?
        .uid();

    Ok(SearchListener {
        listener,
        daemon_uid,
    })
}

fn cleanup_socket_file_at(socket_path: &Path, approved_root: &Path) -> Result<(), OneupError> {
    remove_socket_file(socket_path, approved_root)?;
    Ok(())
}

async fn request_search_at(
    socket_path: &Path,
    project_root: &Path,
    query: &str,
    limit: usize,
) -> Result<Option<(Vec<SearchResult>, Option<String>)>, OneupError> {
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
    ipc::write_json_frame(
        &mut stream,
        &request,
        MAX_DAEMON_REQUEST_BYTES,
        write_deadline(),
    )
    .await?;

    match ipc::read_json_frame(&mut stream, MAX_DAEMON_RESPONSE_BYTES, read_deadline()).await? {
        SearchResponse::Results {
            results,
            daemon_version,
        } => Ok(Some((results, daemon_version))),
        SearchResponse::Unavailable { .. } => Ok(None),
    }
}

fn sanitize_request(request: SearchRequest) -> Result<SearchRequest, OneupError> {
    if request.query.trim().is_empty() {
        return Err(
            DaemonError::RequestError("daemon search query must not be empty".to_string()).into(),
        );
    }
    if request.query.len() > MAX_DAEMON_QUERY_BYTES {
        return Err(DaemonError::RequestError(format!(
            "daemon search query exceeds {MAX_DAEMON_QUERY_BYTES} bytes"
        ))
        .into());
    }

    let project_root = request.project_root.canonicalize().map_err(|err| {
        DaemonError::RequestError(format!(
            "failed to canonicalize daemon project root {}: {err}",
            request.project_root.display()
        ))
    })?;

    Ok(SearchRequest {
        project_root,
        query: request.query,
        limit: request.limit.clamp(1, MAX_SEARCH_RESULTS),
    })
}

fn read_deadline() -> Duration {
    Duration::from_millis(DAEMON_READ_TIMEOUT_MS)
}

fn write_deadline() -> Duration {
    Duration::from_millis(DAEMON_WRITE_TIMEOUT_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_socket_path(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("daemon.sock")
    }

    async fn bind_test_listener(socket_path: &Path) -> Result<SearchListener, OneupError> {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                DaemonError::RequestError(format!(
                    "failed to create test socket dir {}: {err}",
                    parent.display()
                ))
            })?;
        }

        let listener = UnixListener::bind(socket_path).map_err(|err| -> OneupError {
            DaemonError::RequestError(format!(
                "failed to bind test search socket {}: {err}",
                socket_path.display()
            ))
            .into()
        })?;
        let daemon_uid = std::fs::metadata(socket_path)
            .map_err(|err| {
                DaemonError::RequestError(format!(
                    "failed to stat test search socket {}: {err}",
                    socket_path.display()
                ))
            })?
            .uid();

        Ok(SearchListener {
            listener,
            daemon_uid,
        })
    }

    #[tokio::test]
    async fn request_search_returns_results() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = test_socket_path(&tmp);
        let listener = bind_test_listener(&socket_path).await.unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let expected_root = project_root.clone();
        let server = tokio::spawn(async move {
            let mut stream = accept_connection(&listener).await.unwrap().unwrap();
            let request = read_request(&mut stream).await.unwrap();
            assert_eq!(
                request,
                SearchRequest {
                    project_root: expected_root.canonicalize().unwrap(),
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
                    daemon_version: Some("0.1.0".to_string()),
                },
            )
            .await
            .unwrap();
        });

        let (results, daemon_version) = request_search_at(&socket_path, &project_root, "needle", 7)
            .await
            .unwrap()
            .unwrap();

        server.await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_path, "src/lib.rs");
        assert_eq!(daemon_version, Some("0.1.0".to_string()));
    }

    #[tokio::test]
    async fn request_search_returns_none_for_unavailable_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = test_socket_path(&tmp);
        let listener = bind_test_listener(&socket_path).await.unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let server = tokio::spawn(async move {
            let mut stream = accept_connection(&listener).await.unwrap().unwrap();
            let _ = read_request(&mut stream).await.unwrap();
            send_unavailable_response(&mut stream).await.unwrap();
        });

        let results = request_search_at(&socket_path, &project_root, "needle", 7)
            .await
            .unwrap();

        server.await.unwrap();
        assert!(results.is_none());
    }

    #[test]
    fn sanitize_request_clamps_limit_and_canonicalizes_root() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let request = sanitize_request(SearchRequest {
            project_root: project_root.clone(),
            query: "needle".to_string(),
            limit: MAX_SEARCH_RESULTS + 10,
        })
        .unwrap();

        assert_eq!(request.project_root, project_root.canonicalize().unwrap());
        assert_eq!(request.limit, MAX_SEARCH_RESULTS);
    }

    #[test]
    fn search_response_results_serializes_with_daemon_version() {
        let response = SearchResponse::Results {
            results: vec![],
            daemon_version: Some("0.1.0".to_string()),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"daemon_version\":\"0.1.0\""));
        assert!(json.contains("\"status\":\"results\""));
    }

    #[test]
    fn search_response_results_deserializes_without_daemon_version() {
        let json = r#"{"status":"results","results":[]}"#;
        let response: SearchResponse = serde_json::from_str(json).unwrap();
        match response {
            SearchResponse::Results {
                results,
                daemon_version,
            } => {
                assert!(results.is_empty());
                assert!(daemon_version.is_none());
            }
            _ => panic!("expected Results variant"),
        }
    }

    #[tokio::test]
    async fn accept_connection_rejects_mismatched_peer_uid() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = test_socket_path(&tmp);
        let listener = bind_test_listener(&socket_path).await.unwrap();

        let server = tokio::spawn(async move {
            let maybe_stream = accept_connection(&SearchListener {
                listener: listener.listener,
                daemon_uid: u32::MAX,
            })
            .await
            .unwrap();
            assert!(maybe_stream.is_none());
        });

        let mut client = UnixStream::connect(&socket_path).await.unwrap();
        let response: SearchResponse = ipc::read_json_frame(
            &mut client,
            MAX_DAEMON_RESPONSE_BYTES,
            Duration::from_millis(250),
        )
        .await
        .unwrap();

        server.await.unwrap();
        assert!(matches!(
            response,
            SearchResponse::Unavailable { ref reason } if reason == SAFE_UNAVAILABLE_REASON
        ));
    }
}
