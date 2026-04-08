use std::time::Duration;

use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::shared::errors::{DaemonError, OneupError};

pub(crate) fn authorize_same_uid(
    stream: &UnixStream,
    expected_uid: u32,
) -> Result<bool, OneupError> {
    let credentials = stream.peer_cred().map_err(|err| {
        DaemonError::RequestError(format!("failed to read peer credentials: {err}"))
    })?;
    Ok(credentials.uid() == expected_uid)
}

pub(crate) async fn read_json_frame<T>(
    stream: &mut UnixStream,
    max_bytes: usize,
    deadline: Duration,
) -> Result<T, OneupError>
where
    T: DeserializeOwned,
{
    let mut length_bytes = [0u8; 4];
    read_exact_with_timeout(
        stream,
        &mut length_bytes,
        deadline,
        "read search socket frame header",
    )
    .await?;

    let payload_len = u32::from_be_bytes(length_bytes) as usize;
    if payload_len > max_bytes {
        return Err(DaemonError::RequestError(format!(
            "search socket frame exceeds {max_bytes} bytes"
        ))
        .into());
    }

    let mut payload = vec![0u8; payload_len];
    read_exact_with_timeout(
        stream,
        &mut payload,
        deadline,
        "read search socket frame body",
    )
    .await?;

    serde_json::from_slice(&payload).map_err(|err| {
        DaemonError::RequestError(format!("failed to decode search socket payload: {err}")).into()
    })
}

pub(crate) async fn write_json_frame<T>(
    stream: &mut UnixStream,
    value: &T,
    max_bytes: usize,
    deadline: Duration,
) -> Result<(), OneupError>
where
    T: Serialize,
{
    let payload = serde_json::to_vec(value).map_err(|err| {
        DaemonError::RequestError(format!("failed to encode search socket payload: {err}"))
    })?;
    if payload.len() > max_bytes {
        return Err(DaemonError::RequestError(format!(
            "encoded search socket payload exceeds {max_bytes} bytes"
        ))
        .into());
    }

    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        DaemonError::RequestError(
            "encoded search socket payload exceeds u32 frame size".to_string(),
        )
    })?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(&payload);

    write_all_with_timeout(stream, &frame, deadline, "write search socket frame").await?;
    shutdown_with_timeout(stream, deadline, "close search socket writer").await
}

async fn read_exact_with_timeout(
    stream: &mut UnixStream,
    buffer: &mut [u8],
    deadline: Duration,
    operation: &str,
) -> Result<(), OneupError> {
    timeout(deadline, stream.read_exact(buffer))
        .await
        .map_err(|_| {
            DaemonError::RequestError(format!(
                "{operation} timed out after {}ms",
                deadline.as_millis()
            ))
        })?
        .map_err(|err| DaemonError::RequestError(format!("failed to {operation}: {err}")))?;
    Ok(())
}

async fn write_all_with_timeout(
    stream: &mut UnixStream,
    buffer: &[u8],
    deadline: Duration,
    operation: &str,
) -> Result<(), OneupError> {
    timeout(deadline, stream.write_all(buffer))
        .await
        .map_err(|_| {
            DaemonError::RequestError(format!(
                "{operation} timed out after {}ms",
                deadline.as_millis()
            ))
        })?
        .map_err(|err| DaemonError::RequestError(format!("failed to {operation}: {err}")))?;
    Ok(())
}

async fn shutdown_with_timeout(
    stream: &mut UnixStream,
    deadline: Duration,
    operation: &str,
) -> Result<(), OneupError> {
    timeout(deadline, stream.shutdown())
        .await
        .map_err(|_| {
            DaemonError::RequestError(format!(
                "{operation} timed out after {}ms",
                deadline.as_millis()
            ))
        })?
        .map_err(|err| DaemonError::RequestError(format!("failed to {operation}: {err}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use serde::{Deserialize, Serialize};
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    #[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
    struct TestPayload {
        value: String,
    }

    #[tokio::test]
    async fn authorize_same_uid_accepts_matching_peer_uid() {
        let (left, right) = UnixStream::pair().unwrap();
        let uid = left.peer_cred().unwrap().uid();

        assert!(authorize_same_uid(&right, uid).unwrap());
    }

    #[tokio::test]
    async fn authorize_same_uid_rejects_mismatched_peer_uid() {
        let (_left, right) = UnixStream::pair().unwrap();

        assert!(!authorize_same_uid(&right, u32::MAX).unwrap());
    }

    #[tokio::test]
    async fn read_json_frame_round_trips_single_frame_payload() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        let server = tokio::spawn(async move {
            write_json_frame(
                &mut writer,
                &TestPayload {
                    value: "needle".to_string(),
                },
                1024,
                Duration::from_millis(250),
            )
            .await
            .unwrap();
        });

        let payload: TestPayload = read_json_frame(&mut reader, 1024, Duration::from_millis(250))
            .await
            .unwrap();

        server.await.unwrap();
        assert_eq!(
            payload,
            TestPayload {
                value: "needle".to_string()
            }
        );
    }

    #[tokio::test]
    async fn read_json_frame_rejects_oversized_frames() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(&(32u32).to_be_bytes()).await.unwrap();

        let err = read_json_frame::<TestPayload>(&mut reader, 8, Duration::from_millis(250))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("exceeds 8 bytes"));
    }

    #[tokio::test]
    async fn read_json_frame_times_out_on_incomplete_payload() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(&(8u32).to_be_bytes()).await.unwrap();
        writer.write_all(b"abc").await.unwrap();

        let err = read_json_frame::<TestPayload>(&mut reader, 1024, Duration::from_millis(20))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn read_json_frame_rejects_malformed_json_payloads() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(&(8u32).to_be_bytes()).await.unwrap();
        writer.write_all(b"{oops!!}").await.unwrap();
        writer.shutdown().await.unwrap();

        let err = read_json_frame::<TestPayload>(&mut reader, 1024, Duration::from_millis(250))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("failed to decode"));
    }

    #[tokio::test]
    async fn write_json_frame_rejects_payloads_over_the_cap() {
        let (mut writer, _reader) = UnixStream::pair().unwrap();

        let err = write_json_frame(
            &mut writer,
            &TestPayload {
                value: "oversized".repeat(16),
            },
            16,
            Duration::from_millis(250),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("exceeds 16 bytes"));
    }

    #[tokio::test]
    async fn write_all_with_timeout_rejects_stalled_writers() {
        let (mut writer, _reader) = UnixStream::pair().unwrap();
        let payload = vec![b'x'; 16 * 1024 * 1024];

        let err = write_all_with_timeout(
            &mut writer,
            &payload,
            Duration::from_millis(20),
            "write saturated frame",
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("timed out"));
    }
}
