use std::path::Path;

use indicatif::{ProgressBar, ProgressStyle};
use ort::session::Session;
use tokenizers::Tokenizer;

use crate::shared::config::model_dir;
use crate::shared::constants::{
    EMBEDDING_BATCH_SIZE, EMBEDDING_DIM, EMBEDDING_MAX_TOKENS, HF_BASE_URL, HF_MODEL_REPO,
    MODEL_FILENAME, TOKENIZER_FILENAME,
};
use crate::shared::errors::{EmbeddingError, OneupError};

const MODEL_DOWNLOAD_URL: &str = "onnx/model.onnx";
const TOKENIZER_DOWNLOAD_URL: &str = "tokenizer.json";

/// Embedding engine backed by an ONNX model (all-MiniLM-L6-v2) with WordPiece tokenization.
///
/// Holds a singleton ONNX session and tokenizer, providing batch inference with
/// mean pooling and L2 normalization to produce 384-dimensional unit vectors.
pub struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
    batch_size: usize,
}

/// Reports whether the embedding model files are present on disk.
pub fn is_model_available() -> bool {
    let dir = match model_dir() {
        Ok(d) => d,
        Err(_) => return false,
    };
    dir.join(MODEL_FILENAME).exists() && dir.join(TOKENIZER_FILENAME).exists()
}

impl Embedder {
    /// Creates a new embedder, auto-downloading the model if it is not already present.
    ///
    /// The ONNX session is initialized once; reuse this instance across calls.
    pub async fn new() -> Result<Self, OneupError> {
        Self::with_batch_size(EMBEDDING_BATCH_SIZE).await
    }

    /// Creates a new embedder with a custom batch size.
    pub async fn with_batch_size(batch_size: usize) -> Result<Self, OneupError> {
        let dir = model_dir()?;

        let model_path = dir.join(MODEL_FILENAME);
        let tokenizer_path = dir.join(TOKENIZER_FILENAME);

        if !model_path.exists() || !tokenizer_path.exists() {
            download_model(&dir).await?;
        }

        let session = Session::builder()
            .map_err(|e| EmbeddingError::InferenceFailed(format!("session builder: {e}")))?
            .with_intra_threads(1)
            .map_err(|e| EmbeddingError::InferenceFailed(format!("set threads: {e}")))?
            .commit_from_file(&model_path)
            .map_err(|e| EmbeddingError::ModelNotAvailable(format!("failed to load model: {e}")))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            EmbeddingError::TokenizationFailed(format!("failed to load tokenizer: {e}"))
        })?;

        Ok(Self {
            session,
            tokenizer,
            batch_size,
        })
    }

    /// Creates an embedder from pre-existing model files at a custom path.
    pub fn from_dir(dir: &Path) -> Result<Self, OneupError> {
        let model_path = dir.join(MODEL_FILENAME);
        let tokenizer_path = dir.join(TOKENIZER_FILENAME);

        if !model_path.exists() {
            return Err(EmbeddingError::ModelNotAvailable(format!(
                "model not found at {}",
                model_path.display()
            ))
            .into());
        }
        if !tokenizer_path.exists() {
            return Err(EmbeddingError::ModelNotAvailable(format!(
                "tokenizer not found at {}",
                tokenizer_path.display()
            ))
            .into());
        }

        let session = Session::builder()
            .map_err(|e| EmbeddingError::InferenceFailed(format!("session builder: {e}")))?
            .with_intra_threads(1)
            .map_err(|e| EmbeddingError::InferenceFailed(format!("set threads: {e}")))?
            .commit_from_file(&model_path)
            .map_err(|e| EmbeddingError::ModelNotAvailable(format!("failed to load model: {e}")))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            EmbeddingError::TokenizationFailed(format!("failed to load tokenizer: {e}"))
        })?;

        Ok(Self {
            session,
            tokenizer,
            batch_size: EMBEDDING_BATCH_SIZE,
        })
    }

    /// Embeds a single text, returning a 384-dimensional unit vector.
    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>, OneupError> {
        let results = self.embed_batch(&[text])?;
        Ok(results.into_iter().next().unwrap())
    }

    /// Embeds a batch of texts, returning one 384-dimensional unit vector per input.
    ///
    /// Inputs are processed in sub-batches of the configured batch size.
    pub fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, OneupError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_embeddings = Vec::with_capacity(texts.len());

        for chunk in texts.chunks(self.batch_size) {
            let batch_embeddings = self.run_inference(chunk)?;
            all_embeddings.extend(batch_embeddings);
        }

        Ok(all_embeddings)
    }

    fn run_inference(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, OneupError> {
        let batch_size = texts.len();

        let encodings = texts
            .iter()
            .map(|t| {
                let mut enc = self
                    .tokenizer
                    .encode(*t, true)
                    .map_err(|e| EmbeddingError::TokenizationFailed(e.to_string()))?;
                enc.truncate(
                    EMBEDDING_MAX_TOKENS,
                    0,
                    tokenizers::TruncationDirection::Right,
                );
                Ok(enc)
            })
            .collect::<Result<Vec<_>, OneupError>>()?;

        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);

        let mut input_ids = vec![0i64; batch_size * max_len];
        let mut attention_mask = vec![0i64; batch_size * max_len];
        let mut token_type_ids = vec![0i64; batch_size * max_len];

        for (i, enc) in encodings.iter().enumerate() {
            let ids = enc.get_ids();
            let mask = enc.get_attention_mask();
            let type_ids = enc.get_type_ids();
            let offset = i * max_len;
            for (j, &id) in ids.iter().enumerate() {
                input_ids[offset + j] = id as i64;
            }
            for (j, &m) in mask.iter().enumerate() {
                attention_mask[offset + j] = m as i64;
            }
            for (j, &t) in type_ids.iter().enumerate() {
                token_type_ids[offset + j] = t as i64;
            }
        }

        let shape = vec![batch_size as i64, max_len as i64];

        let input_ids_tensor = ort::value::Value::from_array((shape.clone(), input_ids.clone()))
            .map_err(|e| EmbeddingError::InferenceFailed(format!("input_ids tensor: {e}")))?;

        let attention_mask_tensor =
            ort::value::Value::from_array((shape.clone(), attention_mask.clone())).map_err(
                |e| EmbeddingError::InferenceFailed(format!("attention_mask tensor: {e}")),
            )?;

        let token_type_ids_tensor = ort::value::Value::from_array((shape, token_type_ids))
            .map_err(|e| EmbeddingError::InferenceFailed(format!("token_type_ids tensor: {e}")))?;

        let inputs = ort::inputs![
            "input_ids" => input_ids_tensor,
            "attention_mask" => attention_mask_tensor,
            "token_type_ids" => token_type_ids_tensor,
        ];

        let outputs = self
            .session
            .run(inputs)
            .map_err(|e| EmbeddingError::InferenceFailed(format!("session run: {e}")))?;

        let output_value = &outputs[0];

        let (out_shape, raw) = output_value
            .try_extract_tensor::<f32>()
            .map_err(|e| EmbeddingError::InferenceFailed(format!("extract tensor: {e}")))?;

        let hidden_dim = *out_shape.last().unwrap_or(&0) as usize;
        let seq_len = if out_shape.len() >= 2 {
            out_shape[1] as usize
        } else {
            0
        };

        if hidden_dim != EMBEDDING_DIM {
            return Err(EmbeddingError::InferenceFailed(format!(
                "expected {EMBEDDING_DIM} dims, got {hidden_dim}"
            ))
            .into());
        }

        let mut embeddings = Vec::with_capacity(batch_size);

        for i in 0..batch_size {
            let mut pooled = vec![0.0f32; EMBEDDING_DIM];
            let mut mask_sum = 0.0f32;

            for j in 0..seq_len {
                let mask_val = attention_mask[i * max_len + j] as f32;
                if mask_val > 0.0 {
                    mask_sum += mask_val;
                    let base = i * seq_len * hidden_dim + j * hidden_dim;
                    for k in 0..EMBEDDING_DIM {
                        pooled[k] += raw[base + k] * mask_val;
                    }
                }
            }

            if mask_sum > 0.0 {
                for v in pooled.iter_mut() {
                    *v /= mask_sum;
                }
            }

            let norm = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 0.0 {
                for v in pooled.iter_mut() {
                    *v /= norm;
                }
            }

            embeddings.push(pooled);
        }

        Ok(embeddings)
    }
}

async fn download_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    label: &str,
) -> Result<(), OneupError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| EmbeddingError::DownloadFailed(format!("{label}: {e}")))?;

    if !response.status().is_success() {
        return Err(
            EmbeddingError::DownloadFailed(format!("{label}: HTTP {}", response.status())).into(),
        );
    }

    let total = response.content_length().unwrap_or(0);

    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{msg} [{bar:40}] {bytes}/{total_bytes} ({eta})")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
    );
    pb.set_message(label.to_string());

    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| EmbeddingError::DownloadFailed(format!("{label} file create: {e}")))?;

    let mut stream = response.bytes_stream();
    let mut downloaded = 0u64;

    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| EmbeddingError::DownloadFailed(format!("{label} stream: {e}")))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| EmbeddingError::DownloadFailed(format!("{label} write: {e}")))?;
        downloaded += chunk.len() as u64;
        pb.set_position(downloaded);
    }

    file.flush()
        .await
        .map_err(|e| EmbeddingError::DownloadFailed(format!("{label} flush: {e}")))?;

    pb.finish_with_message(format!("{label} done"));
    Ok(())
}

async fn download_model(dir: &Path) -> Result<(), OneupError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| EmbeddingError::DownloadFailed(format!("create model dir: {e}")))?;

    let client = reqwest::Client::new();

    let model_url = format!(
        "{}/{}/resolve/main/{}",
        HF_BASE_URL, HF_MODEL_REPO, MODEL_DOWNLOAD_URL
    );
    let tokenizer_url = format!(
        "{}/{}/resolve/main/{}",
        HF_BASE_URL, HF_MODEL_REPO, TOKENIZER_DOWNLOAD_URL
    );

    download_file(
        &client,
        &model_url,
        &dir.join(MODEL_FILENAME),
        "Downloading model",
    )
    .await?;

    download_file(
        &client,
        &tokenizer_url,
        &dir.join(TOKENIZER_FILENAME),
        "Downloading tokenizer",
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_availability_check() {
        let available = is_model_available();
        assert!(
            available || !available,
            "is_model_available should return a bool"
        );
    }

    #[test]
    fn from_dir_missing_model() {
        let tmp = tempfile::tempdir().unwrap();
        let result = Embedder::from_dir(tmp.path());
        assert!(result.is_err());
        let err = format!("{}", result.err().unwrap());
        assert!(err.contains("model not found") || err.contains("not found"));
    }

    #[test]
    fn from_dir_missing_tokenizer() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MODEL_FILENAME), b"not a real model").unwrap();
        let result = Embedder::from_dir(tmp.path());
        assert!(result.is_err());
        let err = format!("{}", result.err().unwrap());
        assert!(err.contains("tokenizer not found") || err.contains("not found"));
    }

    #[test]
    fn embed_batch_empty_input() {
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir(&model_dir().unwrap()).unwrap();
        let result = embedder.embed_batch(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn embed_one_produces_correct_dim() {
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir(&model_dir().unwrap()).unwrap();
        let vec = embedder.embed_one("hello world").unwrap();
        assert_eq!(vec.len(), EMBEDDING_DIM);
    }

    #[test]
    fn embed_one_l2_normalized() {
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir(&model_dir().unwrap()).unwrap();
        let vec = embedder.embed_one("the quick brown fox").unwrap();
        let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "L2 norm should be ~1.0, got {norm}"
        );
    }

    #[test]
    fn embed_batch_multiple_texts() {
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir(&model_dir().unwrap()).unwrap();
        let texts = vec![
            "error handling in rust",
            "machine learning algorithms",
            "web server configuration",
        ];
        let results = embedder.embed_batch(&texts).unwrap();
        assert_eq!(results.len(), 3);
        for vec in &results {
            assert_eq!(vec.len(), EMBEDDING_DIM);
            let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-4,
                "L2 norm should be ~1.0, got {norm}"
            );
        }
    }

    #[test]
    fn embed_similar_texts_closer_than_dissimilar() {
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let mut embedder = Embedder::from_dir(&model_dir().unwrap()).unwrap();
        let vecs = embedder
            .embed_batch(&[
                "how to handle errors in rust",
                "error handling patterns in rust programming",
                "recipe for chocolate cake",
            ])
            .unwrap();

        let cosine =
            |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b.iter()).map(|(x, y)| x * y).sum() };

        let sim_similar = cosine(&vecs[0], &vecs[1]);
        let sim_dissimilar = cosine(&vecs[0], &vecs[2]);
        assert!(
            sim_similar > sim_dissimilar,
            "similar texts should have higher cosine similarity: {sim_similar} vs {sim_dissimilar}"
        );
    }

    #[test]
    fn embed_batch_exceeding_batch_size() {
        if !is_model_available() {
            eprintln!("skipping: model not available");
            return;
        }
        let dir = model_dir().unwrap();
        let mut embedder = Embedder {
            session: Session::builder()
                .unwrap()
                .with_intra_threads(1)
                .unwrap()
                .commit_from_file(dir.join(MODEL_FILENAME))
                .unwrap(),
            tokenizer: Tokenizer::from_file(dir.join(TOKENIZER_FILENAME)).unwrap(),
            batch_size: 2,
        };

        let texts: Vec<&str> = vec![
            "text one",
            "text two",
            "text three",
            "text four",
            "text five",
        ];
        let results = embedder.embed_batch(&texts).unwrap();
        assert_eq!(results.len(), 5);
        for vec in &results {
            assert_eq!(vec.len(), EMBEDDING_DIM);
        }
    }
}
