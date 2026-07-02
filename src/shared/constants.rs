/// Embedding vector dimensionality (all-MiniLM-L6-v2).
pub const EMBEDDING_DIM: usize = 384;

/// 1up version from Cargo.toml, embedded at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default batch size for embedding inference (R-005).
///
/// Held at 32: a partial best-of-3 reindex benchmark over this repo's own ~1.5k
/// chunk corpus showed 32 (~47.7s) slightly ahead of 64 (~49.6s), i.e. larger
/// batches did not amortize per-call overhead enough to win once each sub-batch's
/// tensor is already trimmed to its real token length (see `Embedder::embed_batch`
/// length-bucketing). The exhaustive {32,64,128} release-LTO sweep is deferred to
/// the feature-level benchmark manual item; 32 is the conservative, measured-best
/// default among the sizes that completed.
pub const EMBEDDING_BATCH_SIZE: usize = 32;

/// Effective maximum token length for the embedding model.
///
/// The shipped `tokenizer.json` hard-pins `truncation.max_length = 128` and
/// `padding = {Fixed: 128}`, so setting this constant alone is a no-op — the
/// file-baked 128 always wins. `Embedder::load_variant` therefore applies a
/// programmatic `with_truncation`/`with_padding` override (raising both to
/// `EMBEDDING_MAX_TOKENS`) right after `Tokenizer::from_file`, which is what
/// actually widens the window; the shipped file is left untouched so its
/// `TOKENIZER_SHA256` is unchanged (see `embedder.rs`, HYP-002). Padding must be
/// overridden alongside truncation: `run_inference` copies `ids[0..max_len]`
/// across a mixed-length sub-batch, so leaving padding at `Fixed(128)` while
/// truncation allows 256-token rows would read past a short row's 128-wide id
/// buffer and panic.
///
/// This widens the window on the v18 re-embed: content that previously lost its
/// tail past 128 tokens now embeds up to 256. It rides the already-bumped
/// (unreleased) `SCHEMA_VERSION = 18` migration, and `max_tokens` is folded into
/// `embedding_content_key` so 128- and 256-window vectors can never mix in the
/// content-addressed pool.
pub const EMBEDDING_MAX_TOKENS: usize = 256;

/// Chunk-segment languages excluded from embedding.
///
/// Structural segments always embed; text-chunked configuration and data
/// formats carry little semantic signal per token. This list is shared by the
/// pipeline's embed decision and the storage coverage counters so reported
/// vector coverage always matches what the pipeline would embed.
pub const NON_EMBEDDABLE_CHUNK_LANGUAGES: [&str; 9] = [
    "json",
    "yaml",
    "toml",
    "protobuf",
    "terraform",
    "sql",
    "config",
    "makefile",
    "dockerfile",
];

/// Default number of vector search prefilter candidates (int8 stage).
///
/// Tuned to 400 for schema v13's FLOAT8 vectors: quantization makes the top-K
/// ranking slightly noisier, so a wider candidate pool gives the RRF reranker
/// enough coverage to recover gold segments that drift out of the top 200 but
/// are still in the right neighbourhood. Doubling K closed the recall gap
/// introduced by the FLOAT32 -> FLOAT8 column shift with no measurable search
/// latency impact. The constant serves both vector paths: it is the LIMIT of
/// the exhaustive-scan path (where candidate cost is one sort, so 400 keeps
/// the reranker pool identical to the index path) and the per-context K for
/// the approximate index path above
/// [`VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS`], where it remains the
/// recall/latency tradeoff originally tuned for quantization noise.
pub const VECTOR_PREFILTER_K: usize = 400;

/// Maximum context-scoped vector count served by the exhaustive-scan path.
///
/// The disk-based approximate vector index answers `vector_top_k` by beam
/// traversal over a neighbor graph, which is read-heavy and pathologically
/// slow — and, contrary to the original "amortizes at scale" assumption, it
/// gets WORSE as the corpus grows, not better: ~7s single-thread CPU over
/// ~4.5k vectors, and ~45s over ~27k vectors (measured on the emdash corpus).
/// The exhaustive path is the inverse: an exact `vector_distance_cos` scan is
/// a single linear pass of ~N x 384 dot products (~6.3M MACs at 16384, well
/// under 10ms) and stays sub-second well past 256k vectors. So the exact scan
/// is preferred for all realistic single-repo corpus sizes; the `vector_top_k`
/// graph path only takes over above this (deliberately high) bound, where the
/// memory-resident linear pass would finally lose to the disk-based index.
pub const VECTOR_EXHAUSTIVE_SCAN_MAX_VECTORS: usize = 262_144;

/// Maximum number of indexed worktree contexts used to scale vector prefiltering.
///
/// libSQL vector search runs against the shared vector index before context
/// filtering, so linked worktrees dilute the active context's candidate share.
/// Scaling by context count preserves recall while bounding worst-case latency.
pub const VECTOR_PREFILTER_CONTEXT_SCALE_LIMIT: usize = 8;

/// RRF fusion constant.
pub const RRF_K: f64 = 60.0;

/// Weight multiplier for vector search scores in RRF fusion.
pub const VECTOR_WEIGHT: f64 = 1.5;

/// Weight multiplier for exact/fuzzy symbol search scores in RRF fusion.
pub const SYMBOL_WEIGHT: f64 = 4.0;

/// Maximum search results returned per query.
pub const MAX_SEARCH_RESULTS: usize = 20;

/// Maximum size of a framed daemon request payload in bytes.
pub const MAX_DAEMON_REQUEST_BYTES: usize = 16 * 1024;

/// Maximum size of a framed daemon response payload in bytes.
pub const MAX_DAEMON_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// Maximum daemon search query length in bytes.
pub const MAX_DAEMON_QUERY_BYTES: usize = 4 * 1024;

/// Read deadline for a single daemon IPC frame.
pub const DAEMON_READ_TIMEOUT_MS: u64 = 250;

/// Write deadline for a single daemon IPC frame.
pub const DAEMON_WRITE_TIMEOUT_MS: u64 = 250;

/// Maximum number of in-flight daemon requests before new clients are shed.
pub const MAX_DAEMON_IN_FLIGHT_REQUESTS: usize = 8;

/// Maximum results per file in search output.
pub const MAX_RESULTS_PER_FILE: usize = 3;

/// Degraded-search reason emitted when the local index holds no vector rows
/// for the active context, so search stays FTS-only without touching the
/// embedder. Shared by the CLI and MCP vectorless gates so degraded-mode
/// wording cannot drift between surfaces.
pub const NO_INDEXED_EMBEDDINGS_REASON: &str =
    "index contains no embeddings for this context; semantic ranking disabled (FTS-only)";

/// Degraded-search reason emitted when a non-destructive index rebuild is in
/// progress and the prior index is still being served (stale-but-available).
/// Carried on the existing `degraded_reason` channel — never on the
/// machine-readable result stream — and combined with any other degraded
/// reason (e.g. [`NO_INDEXED_EMBEDDINGS_REASON`]) rather than replacing it.
/// Single source of truth for this wording so the rebuild/stale notice cannot
/// drift between the CLI and MCP surfaces.
pub const STALE_REBUILD_REASON: &str = "index is rebuilding; results may be stale";

/// Default context expansion window (lines) when tree-sitter is unavailable.
pub const CONTEXT_FALLBACK_LINES: usize = 50;

/// Sliding window size (lines) for text chunker.
pub const CHUNK_WINDOW_SIZE: usize = 60;

/// Sliding window overlap (lines) for text chunker.
pub const CHUNK_OVERLAP: usize = 10;

/// Debounce interval for file watcher events in milliseconds.
pub const WATCHER_DEBOUNCE_MS: u64 = 500;

/// Maximum interval between persisted `index_status.json` progress writes
/// during a single indexing pass (`FlushState::refresh`, T7/REQ-004).
///
/// Before this throttle, every skipped file or stored batch triggered its own
/// `atomic_replace` (temp-write + fsync + rename), which dominated wall time
/// on skip-heavy incremental passes. The in-memory progress bar
/// (`emit_progress`) stays per-event and is unaffected; only the on-disk
/// write is gated. The terminal `Complete` phase always flushes regardless
/// of this gate, so `1up status`/`list`/MCP readers never observe a stale
/// non-terminal state past run completion.
pub const PROGRESS_PERSIST_THROTTLE_MS: u64 = 250;

/// How long the daemon may run with zero registered projects before it
/// self-exits. The daemon otherwise only exits on SIGTERM, so a daemon left
/// behind once its last project is deregistered (`1up stop`), or one orphaned
/// by a crashed/ended parent (e.g. a test run in a throwaway HOME), would
/// linger forever and accumulate. A daemon with any registered project never
/// idles out. Overridable via [`DAEMON_IDLE_SHUTDOWN_ENV_VAR`] for tests and
/// operators; `0` exits as soon as the daemon observes itself empty.
pub const DAEMON_IDLE_SHUTDOWN_SECS: u64 = 60;

/// Runtime override for [`DAEMON_IDLE_SHUTDOWN_SECS`] (seconds).
pub const DAEMON_IDLE_SHUTDOWN_ENV_VAR: &str = "ONEUP_DAEMON_IDLE_SHUTDOWN_SECS";

/// Maximum interval between persisted daemon file-check heartbeats.
pub const DAEMON_FILE_CHECK_PERSIST_INTERVAL_MS: u64 = 30_000;

/// Bounded timeout for a graceful daemon drain (SIGTERM + poll).
///
/// Reused by `1up update`'s pre-update stop and by the post-upgrade
/// version-handshake drain/restart on the search path. ~3s (30 x 100ms order
/// of magnitude) is the conservative bound inherited from the original update
/// stop primitive; it now wraps a genuinely-cancellable indexing pass (the
/// daemon cancels in-flight work on SIGTERM), with the local in-process search
/// as the safety net if a drain still exceeds it.
pub const DAEMON_DRAIN_TIMEOUT_MS: u64 = 3_000;

/// Poll interval while waiting for a drained daemon to exit after SIGTERM.
pub const DAEMON_DRAIN_POLL_INTERVAL_MS: u64 = 100;

/// Whether the post-upgrade daemon auto-restart is gated on an idle/size
/// threshold before draining and restarting on a detected version mismatch.
///
/// Decision (REQ-004, OQ-003): `false` — there is **no** idle/size gating by
/// default. On a detected `daemon_version` mismatch the search path always
/// drains the stale daemon and restarts under the current binary (trigger
/// point: `src/cli/search.rs`). Serving silently wrong-version results is the
/// headline hazard this phase retires, so correctness is preferred over the
/// small risk of interrupting active work; the drain is bounded
/// ([`DAEMON_DRAIN_TIMEOUT_MS`]) and falls back to local in-process search, so
/// an unconditional restart can never strand the user.
///
/// The specific idle/size thresholds are an open owner decision (OQ-003): a
/// future owner can flip this to `true` and add the gating check at the
/// trigger point without re-deriving the rationale recorded here. Provisioned
/// (unused until that owner decision) per the codebase's pre-provisioned-API
/// convention.
#[allow(dead_code)]
pub const DAEMON_AUTO_RESTART_GATING_ENABLED: bool = false;

/// Bounded wait for the single-writer rebuild lock before a synchronous
/// one-shot rebuild (CLI `index`/`reindex`, MCP indexing) fails closed rather
/// than racing a competing rebuild of the shared `.1up/index.db`. The daemon
/// instead acquires the lock non-blockingly and defers the pass, so this bound
/// governs only the user-driven one-shot commands.
pub const REBUILD_LOCK_CONTENTION_TIMEOUT_MS: u64 = 5_000;

/// Poll interval while waiting for a contended rebuild lock to be released.
pub const REBUILD_LOCK_RETRY_INTERVAL_MS: u64 = 200;

/// Number of retries for transient database lock failures.
pub const DB_LOCK_RETRY_ATTEMPTS: usize = 10;

/// Delay between transient database lock retries.
pub const DB_LOCK_RETRY_DELAY_MS: u64 = 50;

/// Write-ahead-log autocheckpoint threshold, in WAL pages, for the write/staging
/// connection profile only.
///
/// SQLite's default `wal_autocheckpoint` (1000 pages) forces frequent
/// mid-rebuild checkpoints during a large cold rebuild, each competing with the
/// embed/write flush. Raising it to 10000 on the staging connection lets the WAL
/// grow further between checkpoints so a full rebuild pays far fewer checkpoint
/// stalls; the staging file is finalized with an explicit `wal_checkpoint(TRUNCATE)`
/// at swap time, so the larger interim WAL is bounded by the rebuild's lifetime.
/// The read/base profile keeps SQLite's default.
pub const STAGING_WAL_AUTOCHECKPOINT_PAGES: u32 = 10_000;

/// Page-cache size for the write/staging connection profile only, in SQLite's
/// negative-KiB form (`-131072` KiB = 128 MiB).
///
/// 128 MiB keeps more index pages hot during a large cold rebuild than the
/// read/base profile's 32 MiB (`-32768`), cutting page churn on the write path.
/// The read/base profile is left unchanged.
pub const STAGING_DB_CACHE_SIZE_KIB: i32 = -131_072;

/// Owner-only permissions for the XDG-managed state directory.
#[allow(dead_code)]
pub const XDG_STATE_DIR_MODE: u32 = 0o700;

/// Owner-only permissions for the project-local `.1up` directory.
#[allow(dead_code)]
pub const PROJECT_STATE_DIR_MODE: u32 = 0o700;

/// Owner-only permissions for security-sensitive state files.
#[allow(dead_code)]
pub const SECURE_STATE_FILE_MODE: u32 = 0o600;

/// Owner-only permissions for daemon socket files after bind.
#[allow(dead_code)]
pub const SECURE_SOCKET_MODE: u32 = 0o600;

/// Upper bound for auto-selected embedding (ONNX intra-op) threads.
///
/// Embedding is the dominant indexing cost and the only sustained CPU work
/// during the serial flush, so the embed phase scales toward physical cores
/// rather than the legacy fixed cap of 4 (R-004). The bound stays coordinated
/// with parse parallelism by
/// [`crate::shared::types::IndexingConfig::default_jobs`], which reserves cores
/// for embedding so `embed_threads + jobs` never exceeds physical cores — ONNX
/// intra-op throughput regresses ~3.5x once the overlapping parse and embed
/// pools over-subscribe. 8 is a benchmark-informed ceiling for the common
/// single-repo host; the effective per-host value is further bounded by the
/// cores left after the parse pool.
pub const MAX_AUTO_EMBED_THREADS: usize = 8;

/// Minimum number of files written per auto-selected storage transaction.
pub const DEFAULT_INDEX_WRITE_BATCH_FILES: usize = 4;

/// Conservative upper bound for auto-selected storage transaction batches.
pub const MAX_AUTO_INDEX_WRITE_BATCH_FILES: usize = 16;

/// Environment variable for parse worker count.
pub const INDEX_JOBS_ENV_VAR: &str = "ONEUP_INDEX_JOBS";

/// Environment variable for ONNX intra-op thread count.
pub const EMBED_THREADS_ENV_VAR: &str = "ONEUP_EMBED_THREADS";

/// Environment variable for storage writer batch sizing.
pub const INDEX_WRITE_BATCH_FILES_ENV_VAR: &str = "ONEUP_INDEX_WRITE_BATCH_FILES";

/// Environment variable that disables embedding model auto-download when set
/// to any non-empty value other than `0`. CI sets this so test suites stay
/// hermetic: no spawned `1up` process may reach the network for model
/// artifacts, and model availability cannot flip mid-suite.
pub const DISABLE_MODEL_DOWNLOADS_ENV_VAR: &str = "ONEUP_DISABLE_MODEL_DOWNLOADS";

/// Environment variable that explicitly selects the embedding model variant for
/// a run (T1). Accepted values are `int8` and `fp32` (case-insensitive);
/// unset or empty resolves to the established default (`int8`). Any other value
/// is a hard error at run start — the selection is deterministic and never
/// silently falls back to the other variant, so an operator can pin and compare
/// the compact and full-precision models reproducibly.
pub const MODEL_VARIANT_ENV_VAR: &str = "ONEUP_MODEL_VARIANT";

/// Schema version for database layout.
///
/// v18: bundles the Phase-2 re-embed migration. The default embedding path is
/// now the dynamic-INT8 model variant ([`MODEL_ONNX_INT8_FILENAME`]), whose
/// identity is folded into the content-addressed embedding `content_key` via
/// [`MODEL_VARIANT_INT8_SUFFIX`]; INT8 and FP32 produce numerically different
/// vectors, so cached embeddings from a v17 (FP32-keyed) index are invalid and
/// must be re-embedded. The required-objects set also shifts: the dead
/// `idx_segments_file_hash` index is dropped and `idx_segment_vectors_content_key`
/// is added for the ANN fan-out join. The physical/numeric layout therefore
/// differs from v17, so older indexes are incompatible and fail closed with
/// `1up reindex` (no in-place migration).
///
/// v17: embeddings are content-addressed. Vector bytes move out of
/// `segment_vectors` into a shared `embedding_pool` keyed by
/// `hash(model_id, embedding_dim, embed_input)` with a reference count;
/// `segment_vectors` becomes a thin `(segment_id, content_key)` reference and
/// the DiskANN index lives on `embedding_pool.embedding_vec`. The physical
/// layout differs from v16, so older indexes are incompatible and require
/// `1up reindex`.
///
/// v16: markdown heading breadcrumbs store cleaned heading text (inline
/// HTML stripped, link text kept, whitespace collapsed). Stored breadcrumbs
/// and the embedding text composed from them change shape, so indexes built
/// at earlier versions are incompatible and require `1up reindex`.
pub const SCHEMA_VERSION: u32 = 18;

/// Context id used by legacy indexing paths until callers pass an explicit worktree context.
pub const DEFAULT_INDEX_CONTEXT_ID: &str = "default";

/// FP32 ONNX model filename (the always-present baseline / fallback artifact).
pub const MODEL_FILENAME: &str = "model.onnx";

/// INT8-quantized ONNX model filename (R-003, T10; integrity-pinned in T4).
///
/// This dynamic-INT8 build of all-MiniLM-L6-v2 is the v18 default CPU embedding
/// path. It is a first-class, integrity-verified artifact: a third entry in the
/// embedder's `EXPECTED_ARTIFACT_FILES` alongside [`MODEL_FILENAME`] and
/// [`TOKENIZER_FILENAME`], provisioned through the same staged-download +
/// pinned-SHA verification + manifest machinery, and re-digested against
/// [`MODEL_ONNX_INT8_SHA256`] at load time. There is no presence-based
/// auto-selection and no cross-variant fallback: variant selection is
/// deterministic (`ONEUP_MODEL_VARIANT` override > default `Int8`), and a
/// corrupt or missing INT8 artifact fails closed (FTS-only degrade with a clear
/// reason) rather than quietly serving the numerically different FP32 model.
pub const MODEL_ONNX_INT8_FILENAME: &str = "model.int8.onnx";

/// Model-identity suffix that distinguishes the INT8 variant from the FP32
/// baseline inside the content-addressed embedding key (R-003, T10).
///
/// The embedding `content_key` and the stored `meta.embedding_model` fold the
/// model identity (`HF_MODEL_REPO`). The INT8 and FP32 builds of the same repo
/// produce numerically different vectors, so they MUST resolve to distinct keys:
/// the active variant's identity is `HF_MODEL_REPO` for FP32 and
/// `format!("{HF_MODEL_REPO}{MODEL_VARIANT_INT8_SUFFIX}")` for INT8. Swapping the
/// variant therefore changes every key, invalidating cached vectors and forcing
/// a clean re-embed (the load-bearing correctness point for the v18 re-embed).
pub const MODEL_VARIANT_INT8_SUFFIX: &str = "@int8";

/// Tokenizer filename.
pub const TOKENIZER_FILENAME: &str = "tokenizer.json";

/// Verified model artifact store directory name.
pub const MODEL_VERIFIED_DIRNAME: &str = "verified";

/// Model artifact staging directory name.
pub const MODEL_STAGING_DIRNAME: &str = ".staging";

/// Active model artifact pointer filename.
pub const MODEL_CURRENT_MANIFEST_FILENAME: &str = "current.json";

/// Verified model artifact manifest filename.
pub const MODEL_ARTIFACT_MANIFEST_FILENAME: &str = "manifest.json";

/// Schema version for verified model artifact metadata.
pub const MODEL_ARTIFACT_MANIFEST_VERSION: u32 = 1;

/// Connect timeout for model downloads.
pub const MODEL_DOWNLOAD_CONNECT_TIMEOUT_SECS: u64 = 10;

/// Total request timeout for model downloads.
pub const MODEL_DOWNLOAD_TIMEOUT_SECS: u64 = 300;

/// Pinned SHA-256 digest for the FP32 ONNX embedding model.
pub const MODEL_ONNX_SHA256: &str =
    "6fd5d72fe4589f189f8ebc006442dbb529bb7ce38f8082112682524616046452";

/// Pinned SHA-256 digest for the INT8-quantized ONNX embedding model (T4).
///
/// The upstream artifact is `onnx/model_qint8_avx512.onnx` in [`HF_MODEL_REPO`]
/// (byte-identical to the repo's arm64 and avx512_vnni INT8 variants — same LFS
/// object). This digest is verified both at download/activation time (like every
/// other artifact) and again at load time in `load_variant(Int8)`, so a
/// post-activation corruption or tamper refuses the model with a clear
/// expected-vs-got error instead of serving embeddings from it (REQ-004).
pub const MODEL_ONNX_INT8_SHA256: &str =
    "4278337fd0ff3c68bfb6291042cad8ab363e1d9fbc43dcb499fe91c871902474";

/// Pinned SHA-256 digest for the tokenizer artifact.
pub const TOKENIZER_SHA256: &str =
    "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037";

/// Hugging Face model repository for auto-download.
pub const HF_MODEL_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";

/// Base URL for Hugging Face model downloads.
pub const HF_BASE_URL: &str = "https://huggingface.co";

/// Build/runtime env var that enables the update manifest endpoint.
///
/// Release builds set this at compile time. A runtime value overrides the
/// baked value; an empty runtime value disables updates for the current
/// process.
pub const UPDATE_MANIFEST_URL_ENV_VAR: &str = "ONEUP_UPDATE_MANIFEST_URL";

/// Filename for the local update-check cache.
pub const UPDATE_CHECK_CACHE_FILENAME: &str = "update-check.json";

/// User-facing message shown when the binary was built without update support.
pub const UPDATE_DISABLED_MESSAGE: &str = "Updates are disabled for this build.";

/// Time-to-live for the update-check cache in seconds (24 hours).
pub const UPDATE_CHECK_TTL_SECS: u64 = 86_400;

/// HTTP request timeout for update manifest fetches in seconds.
pub const UPDATE_CHECK_TIMEOUT_SECS: u64 = 5;

/// TCP connect timeout for update manifest fetches in seconds.
pub const UPDATE_CHECK_CONNECT_TIMEOUT_SECS: u64 = 3;

/// HTTP request timeout for update binary downloads in seconds.
pub const UPDATE_DOWNLOAD_TIMEOUT_SECS: u64 = 300;

/// TCP connect timeout for update binary downloads in seconds.
pub const UPDATE_DOWNLOAD_CONNECT_TIMEOUT_SECS: u64 = 10;

/// Clock-skew tolerance applied to the update-manifest `expiry` gate.
///
/// The self-update path refuses a manifest only once `now > expiry + this
/// skew`, so a machine with a moderately wrong clock is not falsely refused a
/// still-current feed. Deliberately generous (1 day) yet small relative to the
/// release-side expiry TTL (90 days, set in `generate_release_manifest.sh`): it
/// barely weakens freeze/staleness protection while absorbing realistic clock
/// drift, and is paired with an actionable refusal message.
pub const UPDATE_MANIFEST_EXPIRY_CLOCK_SKEW_SECS: u64 = 24 * 60 * 60;

/// GitHub `owner/repo` slug whose release attestations the self-update path
/// trusts. Used to build the attestations-API request path and (via
/// [`ATTESTATION_WORKFLOW_IDENTITY_PREFIX`]) to pin the signing identity.
pub const ATTESTATION_REPO_SLUG: &str = "rp1-run/1up";

/// OIDC issuer the release attestation's signing certificate must carry.
///
/// Keyless-OIDC GitHub Actions provenance is always issued by this token
/// service; pinning it rejects any attestation minted through a different
/// identity provider even if it otherwise chains to the Sigstore root.
pub const ATTESTATION_OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// Required prefix of the attestation certificate's SAN (the signing workflow
/// identity), matching repo + workflow file but deliberately NOT the trailing
/// `@<ref>`.
///
/// GitHub encodes the signer as
/// `https://github.com/<owner>/<repo>/.github/workflows/<wf>.yml@<ref>`, where
/// `<ref>` varies per release (`refs/tags/v0.1.12`, a branch, etc.). Pinning the
/// repo + workflow path while allowing any ref is the granularity
/// `gh attestation verify --repo <owner>/<repo>` enforces: it binds the artifact
/// to *this project's own release workflow* (an attacker cannot mint a cert for
/// it without compromising GitHub's OIDC) without coupling the client to a tag
/// string that legitimately changes every release.
pub const ATTESTATION_WORKFLOW_IDENTITY_PREFIX: &str =
    "https://github.com/rp1-run/1up/.github/workflows/release-assets.yml@";

/// Base URL of the GitHub REST API used to fetch release attestations by digest.
///
/// The self-update path requests
/// `{base}/repos/{slug}/attestations/sha256:{hex}`; the response carries the
/// keyless-OIDC Sigstore bundle(s) GitHub stored for that archive's digest (the
/// attestation is keyed by digest, not uploaded as a release asset).
pub const GITHUB_API_BASE_URL: &str = "https://api.github.com";

/// Frozen set of hosts an artifact download URL (initial request or any
/// redirect hop) is allowed to reach.
///
/// A manifest is TLS-trusted but not yet attestation-verified at the moment
/// its artifact URL is dereferenced (the archive's own attestation gate runs
/// only after download), so a tampered manifest — or a malicious redirect —
/// must not be able to steer the download to an attacker-controlled host.
/// Membership is frozen post-HYP-002 validation, which confirmed the live
/// redirect chain for a release asset is exactly `github.com` ->
/// `release-assets.githubusercontent.com`; `objects.githubusercontent.com` is
/// kept as a historical GitHub release-asset CDN hedge. Deliberately a
/// compile-time constant, not runtime-configurable: an update feed that could
/// redefine its own trust boundary would defeat the allowlist's purpose.
pub const UPDATE_ARTIFACT_HOST_ALLOWLIST: [&str; 3] = [
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// PLACEHOLDER — conservative interim value, **not** a finalized product
/// default. Number of most-recently-updated (`worktree_contexts.updated_at`)
/// same-source contexts `1up gc`'s `SupersededSameSource` retention policy
/// keeps regardless of age; only contexts beyond this rank (and past
/// [`GC_SUPERSEDED_SAME_SOURCE_MAX_AGE_DAYS`]) are eligible for pruning.
/// Governance constraint (full-scan-audit-fixes-warm-path-lifecycle REQ-003):
/// numeric GC defaults must not be invented as final; this value is
/// finalized at the planning gate.
pub const GC_SUPERSEDED_SAME_SOURCE_KEEP_COUNT: usize = 3;

/// PLACEHOLDER — conservative interim value, **not** a finalized product
/// default. Minimum age in days a same-source context ranked beyond
/// [`GC_SUPERSEDED_SAME_SOURCE_KEEP_COUNT`] must reach (by
/// `worktree_contexts.updated_at`) before `1up gc`'s `SupersededSameSource`
/// retention policy considers it prunable. Finalized at the planning gate.
pub const GC_SUPERSEDED_SAME_SOURCE_MAX_AGE_DAYS: i64 = 30;
