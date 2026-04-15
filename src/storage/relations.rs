#![allow(dead_code)]

use std::collections::HashSet;

use libsql::Connection;

use crate::shared::errors::{OneupError, StorageError};
use crate::shared::symbols::normalize_symbolish;
use crate::storage::queries;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationKind {
    Call,
    Reference,
}

impl RelationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Reference => "reference",
        }
    }

    fn from_db(value: &str) -> Result<Self, OneupError> {
        match value {
            "call" => Ok(Self::Call),
            "reference" => Ok(Self::Reference),
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRelation {
    pub source_segment_id: String,
    pub relation_kind: RelationKind,
    pub raw_target_symbol: String,
    pub canonical_target_symbol: String,
    pub lookup_canonical_symbol: String,
    pub qualifier_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelationTargetDescriptor {
    canonical_target_symbol: String,
    lookup_canonical_symbol: String,
    qualifier_fingerprint: String,
}

pub fn build_relation_inserts(
    source_segment_id: &str,
    called_symbols: &[String],
    referenced_symbols: &[String],
) -> Vec<RelationInsert> {
    let mut relations = Vec::new();
    let mut seen = HashSet::new();

    for (relation_kind, symbols) in [
        (RelationKind::Call, called_symbols),
        (RelationKind::Reference, referenced_symbols),
    ] {
        for symbol in symbols {
            let Some(descriptor) = relation_target_descriptor(symbol) else {
                continue;
            };

            let dedupe_key = (relation_kind, descriptor.canonical_target_symbol.clone());
            if seen.insert(dedupe_key) {
                relations.push(RelationInsert {
                    source_segment_id: source_segment_id.to_string(),
                    relation_kind,
                    raw_target_symbol: symbol.clone(),
                    canonical_target_symbol: descriptor.canonical_target_symbol,
                    lookup_canonical_symbol: descriptor.lookup_canonical_symbol,
                    qualifier_fingerprint: descriptor.qualifier_fingerprint,
                });
            }
        }
    }

    relations
}

fn relation_target_descriptor(raw_target_symbol: &str) -> Option<RelationTargetDescriptor> {
    let canonical_target_symbol = normalize_symbolish(raw_target_symbol);
    if canonical_target_symbol.is_empty() {
        return None;
    }

    let components: Vec<String> = raw_target_symbol
        .split(is_symbol_component_separator)
        .map(normalize_symbolish)
        .filter(|component| !component.is_empty())
        .collect();

    let lookup_canonical_symbol = components
        .last()
        .cloned()
        .unwrap_or_else(|| canonical_target_symbol.clone());
    let qualifier_fingerprint = components
        .iter()
        .take(components.len().saturating_sub(1))
        .cloned()
        .collect::<Vec<_>>()
        .join("/");

    Some(RelationTargetDescriptor {
        canonical_target_symbol,
        lookup_canonical_symbol,
        qualifier_fingerprint,
    })
}

fn is_symbol_component_separator(ch: char) -> bool {
    !ch.is_alphanumeric() && ch != '_'
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
    for relation in relations {
        conn.execute(
            queries::INSERT_SEGMENT_RELATION,
            libsql::params![
                relation.source_segment_id.clone(),
                relation.relation_kind.as_str(),
                relation.raw_target_symbol.clone(),
                relation.canonical_target_symbol.clone(),
                relation.lookup_canonical_symbol.clone(),
                relation.qualifier_fingerprint.clone(),
            ],
        )
        .await
        .map_err(|e| StorageError::Query(format!("insert segment relation failed: {e}")))?;
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
            called_symbols: "[]".to_string(),
            file_hash: format!("hash-{id}"),
        }
    }

    #[test]
    fn build_relation_inserts_dedupes_by_kind_and_canonical_symbol() {
        let called_symbols = vec![
            "load_config".to_string(),
            "load_config".to_string(),
            " ".to_string(),
        ];
        let referenced_symbols = vec![
            "ConfigLoader".to_string(),
            "config_loader".to_string(),
            "".to_string(),
        ];

        let relations = build_relation_inserts("seg-1", &called_symbols, &referenced_symbols);

        assert_eq!(
            relations,
            vec![
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Call,
                    raw_target_symbol: "load_config".to_string(),
                    canonical_target_symbol: "loadconfig".to_string(),
                    lookup_canonical_symbol: "loadconfig".to_string(),
                    qualifier_fingerprint: String::new(),
                },
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Reference,
                    raw_target_symbol: "ConfigLoader".to_string(),
                    canonical_target_symbol: "configloader".to_string(),
                    lookup_canonical_symbol: "configloader".to_string(),
                    qualifier_fingerprint: String::new(),
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
        source_a.referenced_symbols = r#"["ConfigLoader"]"#.to_string();
        segments::upsert_segment(&conn, &source_a).await.unwrap();

        let mut source_b = test_segment("source_b", "src/b.rs");
        source_b.called_symbols = r#"["auth.config.load_config"]"#.to_string();
        source_b.referenced_symbols = r#"["ConfigLoader","Settings"]"#.to_string();
        segments::upsert_segment(&conn, &source_b).await.unwrap();

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
        assert_eq!(outbound[0].qualifier_fingerprint, "crate/auth/config");
        assert_eq!(outbound[1].relation_kind, RelationKind::Call);
        assert_eq!(outbound[1].canonical_target_symbol, "writeconfig");
        assert_eq!(outbound[1].lookup_canonical_symbol, "writeconfig");
        assert!(outbound[1].qualifier_fingerprint.is_empty());

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

        let empty = get_outbound_relations(&conn, "source_a", None, 0)
            .await
            .unwrap();
        assert!(empty.is_empty());
    }
}
