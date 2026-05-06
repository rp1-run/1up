use std::path::Path;

use crate::shared::errors::OneupError;
use crate::shared::types::SearchResult;

pub(crate) async fn request_search(
    _project_root: &Path,
    _source_root: &Path,
    _context_id: &str,
    _query: &str,
    _limit: usize,
) -> Result<Option<(Vec<SearchResult>, Option<String>, Option<String>)>, OneupError> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_search_returns_local_fallback_signal() {
        let results = request_search(Path::new("."), Path::new("."), "ctx", "needle", 5)
            .await
            .unwrap();
        assert!(results.is_none());
    }
}
