#![allow(dead_code)]

use std::collections::HashSet;
use std::fmt::Write;

use libsql::Connection;

use crate::shared::errors::{OneupError, StorageError};
use crate::shared::symbols::{
    normalize_edge_identity_kind, normalize_symbolish, owner_fingerprint_from_components,
    split_symbol_components,
};
use crate::shared::types::{ParsedRelation, ParsedRelationKind};
use crate::storage::queries;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationKind {
    Call,
    Reference,
    Conformance,
}

impl RelationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Reference => "reference",
            Self::Conformance => "conformance",
        }
    }

    fn from_db(value: &str) -> Result<Self, OneupError> {
        match value {
            "call" => Ok(Self::Call),
            "reference" => Ok(Self::Reference),
            "conformance" => Ok(Self::Conformance),
            _ => Err(StorageError::Query(format!("unknown relation kind '{value}'")).into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationInsert {
    pub source_segment_id: String,
    pub relation_kind: RelationKind,
    pub raw_target_symbol: String,
    pub canonical_target_symbol: String,
    pub lookup_canonical_symbol: String,
    pub qualifier_fingerprint: String,
    pub edge_identity_kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRelation {
    pub source_segment_id: String,
    pub relation_kind: RelationKind,
    pub raw_target_symbol: String,
    pub canonical_target_symbol: String,
    pub lookup_canonical_symbol: String,
    pub qualifier_fingerprint: String,
    pub edge_identity_kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelationTargetDescriptor {
    canonical_target_symbol: String,
    lookup_canonical_symbol: String,
    qualifier_fingerprint: String,
}

pub fn build_relation_inserts(
    source_segment_id: &str,
    called_relations: &[ParsedRelation],
    referenced_relations: &[ParsedRelation],
) -> Vec<RelationInsert> {
    let mut relations = Vec::new();
    let mut seen = HashSet::new();

    for (relation_kind, relation_group) in [
        (RelationKind::Call, called_relations),
        (RelationKind::Reference, referenced_relations),
    ] {
        for relation in relation_group {
            let relation_kind = relation_kind_for(relation, relation_kind);
            let Some(descriptor) = relation_target_descriptor(&relation.symbol) else {
                continue;
            };
            let edge_identity_kind = normalize_edge_identity_kind(&relation.edge_identity_kind);

            let dedupe_key = (
                relation_kind,
                descriptor.lookup_canonical_symbol.clone(),
                descriptor.qualifier_fingerprint.clone(),
                edge_identity_kind.clone(),
            );
            if seen.insert(dedupe_key) {
                relations.push(RelationInsert {
                    source_segment_id: source_segment_id.to_string(),
                    relation_kind,
                    raw_target_symbol: relation.symbol.clone(),
                    canonical_target_symbol: descriptor.canonical_target_symbol,
                    lookup_canonical_symbol: descriptor.lookup_canonical_symbol,
                    qualifier_fingerprint: descriptor.qualifier_fingerprint,
                    edge_identity_kind,
                });
            }
        }
    }

    relations
}

fn relation_kind_for(relation: &ParsedRelation, default_kind: RelationKind) -> RelationKind {
    match relation.kind {
        Some(ParsedRelationKind::Call) => RelationKind::Call,
        Some(ParsedRelationKind::Reference) => RelationKind::Reference,
        Some(ParsedRelationKind::Conformance) => RelationKind::Conformance,
        None => default_kind,
    }
}

fn relation_target_descriptor(raw_target_symbol: &str) -> Option<RelationTargetDescriptor> {
    let canonical_target_symbol = normalize_symbolish(raw_target_symbol);
    if canonical_target_symbol.is_empty() {
        return None;
    }

    let components = split_symbol_components(raw_target_symbol);

    let lookup_canonical_symbol = components
        .last()
        .cloned()
        .unwrap_or_else(|| canonical_target_symbol.clone());
    let qualifier_fingerprint = owner_fingerprint_from_components(&components);

    Some(RelationTargetDescriptor {
        canonical_target_symbol,
        lookup_canonical_symbol,
        qualifier_fingerprint,
    })
}

pub async fn get_outbound_relations(
    conn: &Connection,
    source_segment_id: &str,
    relation_kind: Option<RelationKind>,
    limit: usize,
) -> Result<Vec<StoredRelation>, OneupError> {
    let Some(limit) = relation_limit(limit)? else {
        return Ok(Vec::new());
    };

    let mut rows = match relation_kind {
        Some(relation_kind) => conn
            .query(
                queries::SELECT_OUTBOUND_RELATIONS_BY_KIND,
                libsql::params![source_segment_id, relation_kind.as_str(), limit],
            )
            .await
            .map_err(|e| StorageError::Query(format!("outbound relation lookup failed: {e}")))?,
        None => conn
            .query(
                queries::SELECT_OUTBOUND_RELATIONS,
                libsql::params![source_segment_id, limit],
            )
            .await
            .map_err(|e| StorageError::Query(format!("outbound relation lookup failed: {e}")))?,
    };

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("outbound relation row iteration failed: {e}")))?
    {
        results.push(row_to_stored_relation(&row)?);
    }

    Ok(results)
}

pub async fn get_inbound_relations(
    conn: &Connection,
    canonical_target_symbol: &str,
    relation_kind: Option<RelationKind>,
    limit: usize,
) -> Result<Vec<StoredRelation>, OneupError> {
    let Some(limit) = relation_limit(limit)? else {
        return Ok(Vec::new());
    };

    let mut rows = match relation_kind {
        Some(relation_kind) => conn
            .query(
                queries::SELECT_INBOUND_RELATIONS_BY_KIND,
                libsql::params![canonical_target_symbol, relation_kind.as_str(), limit],
            )
            .await
            .map_err(|e| StorageError::Query(format!("inbound relation lookup failed: {e}")))?,
        None => conn
            .query(
                queries::SELECT_INBOUND_RELATIONS,
                libsql::params![canonical_target_symbol, limit],
            )
            .await
            .map_err(|e| StorageError::Query(format!("inbound relation lookup failed: {e}")))?,
    };

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("inbound relation row iteration failed: {e}")))?
    {
        results.push(row_to_stored_relation(&row)?);
    }

    Ok(results)
}

#[allow(dead_code)]
pub async fn get_inbound_relations_by_lookup_symbol(
    conn: &Connection,
    lookup_canonical_symbol: &str,
    relation_kind: Option<RelationKind>,
    limit: usize,
) -> Result<Vec<StoredRelation>, OneupError> {
    let Some(limit) = relation_limit(limit)? else {
        return Ok(Vec::new());
    };

    let mut rows = match relation_kind {
        Some(relation_kind) => conn
            .query(
                queries::SELECT_INBOUND_RELATIONS_BY_LOOKUP_SYMBOL_AND_KIND,
                libsql::params![lookup_canonical_symbol, relation_kind.as_str(), limit],
            )
            .await
            .map_err(|e| {
                StorageError::Query(format!("inbound lookup relation lookup failed: {e}"))
            })?,
        None => conn
            .query(
                queries::SELECT_INBOUND_RELATIONS_BY_LOOKUP_SYMBOL,
                libsql::params![lookup_canonical_symbol, limit],
            )
            .await
            .map_err(|e| {
                StorageError::Query(format!("inbound lookup relation lookup failed: {e}"))
            })?,
    };

    let mut results = Vec::new();
    while let Some(row) = rows.next().await.map_err(|e| {
        StorageError::Query(format!("inbound lookup relation row iteration failed: {e}"))
    })? {
        results.push(row_to_stored_relation(&row)?);
    }

    Ok(results)
}

pub(crate) async fn insert_relations(
    conn: &Connection,
    relations: &[RelationInsert],
) -> Result<(), OneupError> {
    if relations.is_empty() {
        return Ok(());
    }

    for chunk in relations.chunks(queries::RELATION_CHUNK_SIZE) {
        let mut sql = String::from(
            "INSERT OR REPLACE INTO segment_relations (\
             source_segment_id, relation_kind, raw_target_symbol, \
             canonical_target_symbol, lookup_canonical_symbol, \
             qualifier_fingerprint, edge_identity_kind, created_at\
             ) VALUES ",
        );
        let mut params: Vec<libsql::Value> =
            Vec::with_capacity(chunk.len() * queries::RELATION_INSERT_COLS);

        for (i, relation) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            let base = i * queries::RELATION_INSERT_COLS;
            write!(
                sql,
                "(?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, datetime('now'))",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
            )
            .expect("write to String cannot fail");

            params.push(relation.source_segment_id.clone().into());
            params.push(relation.relation_kind.as_str().to_string().into());
            params.push(relation.raw_target_symbol.clone().into());
            params.push(relation.canonical_target_symbol.clone().into());
            params.push(relation.lookup_canonical_symbol.clone().into());
            params.push(relation.qualifier_fingerprint.clone().into());
            params.push(relation.edge_identity_kind.clone().into());
        }

        conn.execute(&sql, params).await.map_err(|e| {
            StorageError::Query(format!("batch insert segment relations failed: {e}"))
        })?;
    }

    Ok(())
}

pub(crate) async fn replace_segment_relations(
    conn: &Connection,
    source_segment_id: &str,
    relations: &[RelationInsert],
) -> Result<(), OneupError> {
    validate_relation_source_ids(source_segment_id, relations)?;
    delete_segment_relations_by_source_segment_id(conn, source_segment_id).await?;
    insert_relations(conn, relations).await
}

pub(crate) async fn delete_relations_by_file(
    conn: &Connection,
    file_path: &str,
) -> Result<u64, OneupError> {
    conn.execute(queries::DELETE_SEGMENT_RELATIONS_BY_FILE, [file_path])
        .await
        .map_err(|e| StorageError::Query(format!("delete segment relations by file failed: {e}")))
        .map_err(Into::into)
}

fn relation_limit(limit: usize) -> Result<Option<i64>, OneupError> {
    if limit == 0 {
        return Ok(None);
    }

    i64::try_from(limit).map(Some).map_err(|_| {
        StorageError::Query(format!("relation limit {limit} exceeds i64 range")).into()
    })
}

fn validate_relation_source_ids(
    source_segment_id: &str,
    relations: &[RelationInsert],
) -> Result<(), OneupError> {
    for relation in relations {
        if relation.source_segment_id != source_segment_id {
            return Err(StorageError::Transaction(format!(
                "relation replace for '{source_segment_id}' received row for '{}'",
                relation.source_segment_id
            ))
            .into());
        }
    }

    Ok(())
}

async fn delete_segment_relations_by_source_segment_id(
    conn: &Connection,
    source_segment_id: &str,
) -> Result<u64, OneupError> {
    conn.execute(
        queries::DELETE_SEGMENT_RELATIONS_BY_SOURCE_SEGMENT_ID,
        [source_segment_id],
    )
    .await
    .map_err(|e| StorageError::Query(format!("delete segment relations failed: {e}")))
    .map_err(Into::into)
}

fn row_to_stored_relation(row: &libsql::Row) -> Result<StoredRelation, OneupError> {
    let relation_kind: String = row
        .get(1)
        .map_err(|e| StorageError::Query(format!("read relation_kind failed: {e}")))?;

    Ok(StoredRelation {
        source_segment_id: row
            .get(0)
            .map_err(|e| StorageError::Query(format!("read source_segment_id failed: {e}")))?,
        relation_kind: RelationKind::from_db(&relation_kind)?,
        raw_target_symbol: row
            .get(2)
            .map_err(|e| StorageError::Query(format!("read raw_target_symbol failed: {e}")))?,
        canonical_target_symbol: row.get(3).map_err(|e| {
            StorageError::Query(format!("read canonical_target_symbol failed: {e}"))
        })?,
        lookup_canonical_symbol: row.get(4).map_err(|e| {
            StorageError::Query(format!("read lookup_canonical_symbol failed: {e}"))
        })?,
        qualifier_fingerprint: row
            .get(5)
            .map_err(|e| StorageError::Query(format!("read qualifier_fingerprint failed: {e}")))?,
        edge_identity_kind: row
            .get(6)
            .map_err(|e| StorageError::Query(format!("read edge_identity_kind failed: {e}")))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::symbols::{
        EDGE_IDENTITY_BARE_IDENTIFIER, EDGE_IDENTITY_METHOD_RECEIVER, EDGE_IDENTITY_QUALIFIED_PATH,
    };
    use crate::storage::{
        db::Db,
        schema,
        segments::{self, SegmentInsert},
    };

    async fn setup() -> (Db, Connection) {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        (db, conn)
    }

    fn test_segment(id: &str, file_path: &str) -> SegmentInsert {
        SegmentInsert {
            id: id.to_string(),
            file_path: file_path.to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: format!("fn {id}() {{ }}"),
            line_start: 1,
            line_end: 3,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "IMPLEMENTATION".to_string(),
            defined_symbols: format!("[\"{id}\"]"),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: format!("hash-{id}"),
        }
    }

    fn relation(symbol: &str, edge_identity_kind: &str) -> ParsedRelation {
        ParsedRelation {
            symbol: symbol.to_string(),
            edge_identity_kind: edge_identity_kind.to_string(),
            kind: None,
        }
    }

    fn conformance_relation(symbol: &str, edge_identity_kind: &str) -> ParsedRelation {
        ParsedRelation {
            symbol: symbol.to_string(),
            edge_identity_kind: edge_identity_kind.to_string(),
            kind: Some(ParsedRelationKind::Conformance),
        }
    }

    #[test]
    fn build_relation_inserts_preserves_distinct_edge_identity_and_conformance_kind() {
        let called_relations = vec![
            relation(
                "crate::auth::config::load_config",
                EDGE_IDENTITY_QUALIFIED_PATH,
            ),
            relation("auth.config.load_config", EDGE_IDENTITY_QUALIFIED_PATH),
            relation("service.client.fetch", EDGE_IDENTITY_METHOD_RECEIVER),
            relation("service.client.fetch", EDGE_IDENTITY_BARE_IDENTIFIER),
            relation(" ", EDGE_IDENTITY_BARE_IDENTIFIER),
        ];
        let referenced_relations = vec![
            relation("ConfigLoader", EDGE_IDENTITY_BARE_IDENTIFIER),
            relation("config_loader", EDGE_IDENTITY_BARE_IDENTIFIER),
            conformance_relation("crate::auth::Validator", EDGE_IDENTITY_QUALIFIED_PATH),
            relation("", EDGE_IDENTITY_BARE_IDENTIFIER),
        ];

        let relations = build_relation_inserts("seg-1", &called_relations, &referenced_relations);

        assert_eq!(
            relations,
            vec![
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Call,
                    raw_target_symbol: "crate::auth::config::load_config".to_string(),
                    canonical_target_symbol: "crateauthconfigloadconfig".to_string(),
                    lookup_canonical_symbol: "loadconfig".to_string(),
                    qualifier_fingerprint: "auth/config".to_string(),
                    edge_identity_kind: EDGE_IDENTITY_QUALIFIED_PATH.to_string(),
                },
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Call,
                    raw_target_symbol: "service.client.fetch".to_string(),
                    canonical_target_symbol: "serviceclientfetch".to_string(),
                    lookup_canonical_symbol: "fetch".to_string(),
                    qualifier_fingerprint: "service/client".to_string(),
                    edge_identity_kind: EDGE_IDENTITY_METHOD_RECEIVER.to_string(),
                },
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Call,
                    raw_target_symbol: "service.client.fetch".to_string(),
                    canonical_target_symbol: "serviceclientfetch".to_string(),
                    lookup_canonical_symbol: "fetch".to_string(),
                    qualifier_fingerprint: "service/client".to_string(),
                    edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                },
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Reference,
                    raw_target_symbol: "ConfigLoader".to_string(),
                    canonical_target_symbol: "configloader".to_string(),
                    lookup_canonical_symbol: "configloader".to_string(),
                    qualifier_fingerprint: String::new(),
                    edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                },
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Conformance,
                    raw_target_symbol: "crate::auth::Validator".to_string(),
                    canonical_target_symbol: "crateauthvalidator".to_string(),
                    lookup_canonical_symbol: "validator".to_string(),
                    qualifier_fingerprint: "auth".to_string(),
                    edge_identity_kind: EDGE_IDENTITY_QUALIFIED_PATH.to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn relation_lookup_helpers_filter_and_bound_results() {
        let (_db, conn) = setup().await;

        let mut source_a = test_segment("source_a", "src/a.rs");
        source_a.called_symbols =
            r#"["crate::auth::config::load_config","write_config"]"#.to_string();
        source_a.called_relations = serde_json::to_string(&vec![
            relation(
                "crate::auth::config::load_config",
                EDGE_IDENTITY_QUALIFIED_PATH,
            ),
            relation("write_config", EDGE_IDENTITY_BARE_IDENTIFIER),
        ])
        .unwrap();
        source_a.referenced_symbols = r#"["ConfigLoader"]"#.to_string();
        source_a.referenced_relations = serde_json::to_string(&vec![relation(
            "ConfigLoader",
            EDGE_IDENTITY_BARE_IDENTIFIER,
        )])
        .unwrap();
        segments::upsert_segment(&conn, &source_a).await.unwrap();

        let mut source_b = test_segment("source_b", "src/b.rs");
        source_b.called_symbols = r#"["auth.config.load_config"]"#.to_string();
        source_b.called_relations = serde_json::to_string(&vec![relation(
            "auth.config.load_config",
            EDGE_IDENTITY_QUALIFIED_PATH,
        )])
        .unwrap();
        source_b.referenced_symbols = r#"["ConfigLoader","Settings"]"#.to_string();
        source_b.referenced_relations = serde_json::to_string(&vec![
            relation("ConfigLoader", EDGE_IDENTITY_BARE_IDENTIFIER),
            relation("Settings", EDGE_IDENTITY_BARE_IDENTIFIER),
        ])
        .unwrap();
        segments::upsert_segment(&conn, &source_b).await.unwrap();

        let mut source_c = test_segment("source_c", "src/c.rs");
        source_c.defined_symbols = r#"["AuthStore"]"#.to_string();
        source_c.referenced_symbols = r#"["SessionStore"]"#.to_string();
        source_c.referenced_relations = serde_json::to_string(&vec![conformance_relation(
            "contracts.SessionStore",
            EDGE_IDENTITY_QUALIFIED_PATH,
        )])
        .unwrap();
        segments::upsert_segment(&conn, &source_c).await.unwrap();

        let outbound = get_outbound_relations(&conn, "source_a", None, 2)
            .await
            .unwrap();
        assert_eq!(outbound.len(), 2);
        assert_eq!(outbound[0].relation_kind, RelationKind::Call);
        assert_eq!(
            outbound[0].canonical_target_symbol,
            "crateauthconfigloadconfig"
        );
        assert_eq!(outbound[0].lookup_canonical_symbol, "loadconfig");
        assert_eq!(outbound[0].qualifier_fingerprint, "auth/config");
        assert_eq!(outbound[0].edge_identity_kind, EDGE_IDENTITY_QUALIFIED_PATH);
        assert_eq!(outbound[1].relation_kind, RelationKind::Call);
        assert_eq!(outbound[1].canonical_target_symbol, "writeconfig");
        assert_eq!(outbound[1].lookup_canonical_symbol, "writeconfig");
        assert!(outbound[1].qualifier_fingerprint.is_empty());
        assert_eq!(
            outbound[1].edge_identity_kind,
            EDGE_IDENTITY_BARE_IDENTIFIER
        );

        let inbound =
            get_inbound_relations(&conn, "configloader", Some(RelationKind::Reference), 8)
                .await
                .unwrap();
        assert_eq!(
            inbound
                .iter()
                .map(|relation| relation.source_segment_id.as_str())
                .collect::<Vec<_>>(),
            vec!["source_a", "source_b"]
        );

        let lookup_inbound = get_inbound_relations_by_lookup_symbol(
            &conn,
            "loadconfig",
            Some(RelationKind::Call),
            8,
        )
        .await
        .unwrap();
        assert_eq!(
            lookup_inbound
                .iter()
                .map(|relation| relation.source_segment_id.as_str())
                .collect::<Vec<_>>(),
            vec!["source_a", "source_b"]
        );

        let conformance_inbound = get_inbound_relations_by_lookup_symbol(
            &conn,
            "sessionstore",
            Some(RelationKind::Conformance),
            8,
        )
        .await
        .unwrap();
        assert_eq!(
            conformance_inbound
                .iter()
                .map(|relation| relation.source_segment_id.as_str())
                .collect::<Vec<_>>(),
            vec!["source_c"]
        );

        let empty = get_outbound_relations(&conn, "source_a", None, 0)
            .await
            .unwrap();
        assert!(empty.is_empty());
    }
}
