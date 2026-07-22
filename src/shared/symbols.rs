pub const EDGE_IDENTITY_BARE_IDENTIFIER: &str = "bare_identifier";
pub const EDGE_IDENTITY_QUALIFIED_PATH: &str = "qualified_path";
pub const EDGE_IDENTITY_MEMBER_ACCESS: &str = "member_access";
pub const EDGE_IDENTITY_METHOD_RECEIVER: &str = "method_receiver";
pub const EDGE_IDENTITY_CONSTRUCTOR_LIKE: &str = "constructor_like";
pub const EDGE_IDENTITY_MACRO_LIKE: &str = "macro_like";
pub const EDGE_IDENTITY_DOC_MENTION: &str = "doc_mention";

const LOW_INFORMATION_OWNER_COMPONENTS: &[&str] = &[
    "crate", "self", "super", "this", "src", "lib", "mod", "index", "main", "tests", "test",
    "spec", "rs", "py", "js", "jsx", "ts", "tsx", "go", "java", "c", "cpp", "cc", "cxx", "h",
    "hpp", "hh", "hxx", "kt", "kts",
];

pub fn normalize_symbolish(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

pub fn split_symbol_components(value: &str) -> Vec<String> {
    value
        .split(is_symbol_component_separator)
        .map(normalize_symbolish)
        .filter(|component| !component.is_empty())
        .collect()
}

pub fn clean_owner_components(components: &[String]) -> Vec<String> {
    let mut cleaned = components
        .iter()
        .map(|component| normalize_symbolish(component))
        .filter(|component| {
            !component.is_empty() && !LOW_INFORMATION_OWNER_COMPONENTS.contains(&component.as_str())
        })
        .collect::<Vec<_>>();
    cleaned.dedup();
    cleaned
}

pub fn owner_fingerprint_from_components(components: &[String]) -> String {
    owner_components(components).join("/")
}

pub fn normalize_edge_identity_kind(value: &str) -> String {
    match normalize_symbolish(value).as_str() {
        "qualifiedpath"
        | "scopedidentifier"
        | "qualifiedidentifier"
        | "qualifiedname"
        | "namespaceidentifier"
        | "scoperesolution"
        | "pathexpression" => EDGE_IDENTITY_QUALIFIED_PATH.to_string(),
        "memberaccess"
        | "fieldexpression"
        | "memberexpression"
        | "fieldaccess"
        | "navigationexpression"
        | "attribute" => EDGE_IDENTITY_MEMBER_ACCESS.to_string(),
        "methodreceiver" | "methodcallexpression" | "methodinvocation" => {
            EDGE_IDENTITY_METHOD_RECEIVER.to_string()
        }
        "constructorlike" | "newexpression" | "objectcreationexpression" => {
            EDGE_IDENTITY_CONSTRUCTOR_LIKE.to_string()
        }
        "macrolike" | "macroinvocation" => EDGE_IDENTITY_MACRO_LIKE.to_string(),
        "docmention" => EDGE_IDENTITY_DOC_MENTION.to_string(),
        _ => EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
    }
}

fn is_symbol_component_separator(ch: char) -> bool {
    !ch.is_alphanumeric() && ch != '_'
}

fn owner_components(components: &[String]) -> Vec<String> {
    let mut owner_components = clean_owner_components(components);
    if owner_components.len() <= 1 {
        return Vec::new();
    }
    owner_components.pop();
    owner_components
}

#[cfg(test)]
mod tests {
    use super::{
        clean_owner_components, normalize_edge_identity_kind, normalize_symbolish,
        owner_fingerprint_from_components, split_symbol_components, EDGE_IDENTITY_BARE_IDENTIFIER,
        EDGE_IDENTITY_CONSTRUCTOR_LIKE, EDGE_IDENTITY_DOC_MENTION, EDGE_IDENTITY_MACRO_LIKE,
        EDGE_IDENTITY_MEMBER_ACCESS, EDGE_IDENTITY_METHOD_RECEIVER, EDGE_IDENTITY_QUALIFIED_PATH,
    };

    #[test]
    fn normalizes_symbolish_queries() {
        assert_eq!(normalize_symbolish("ConfigLoader"), "configloader");
        assert_eq!(normalize_symbolish("config_loader"), "configloader");
        assert_eq!(normalize_symbolish("Config Loader"), "configloader");
    }

    #[test]
    fn splits_symbol_components_and_derives_owner_fingerprint() {
        let components = split_symbol_components("crate::auth::config::load_config");
        assert_eq!(components, vec!["crate", "auth", "config", "loadconfig"]);
        assert_eq!(
            clean_owner_components(&components),
            vec!["auth", "config", "loadconfig"]
        );
        assert_eq!(
            owner_fingerprint_from_components(&split_symbol_components("src/search/impact.rs")),
            "search"
        );
    }

    #[test]
    fn normalize_edge_identity_kind_preserves_doc_mention() {
        assert_eq!(
            normalize_edge_identity_kind("doc_mention"),
            EDGE_IDENTITY_DOC_MENTION
        );
        assert_ne!(
            normalize_edge_identity_kind("doc_mention"),
            EDGE_IDENTITY_BARE_IDENTIFIER
        );
    }

    #[test]
    fn normalizes_edge_identity_kinds() {
        assert_eq!(
            normalize_edge_identity_kind("scoped_identifier"),
            EDGE_IDENTITY_QUALIFIED_PATH
        );
        assert_eq!(
            normalize_edge_identity_kind("member_expression"),
            EDGE_IDENTITY_MEMBER_ACCESS
        );
        assert_eq!(
            normalize_edge_identity_kind("method_call_expression"),
            EDGE_IDENTITY_METHOD_RECEIVER
        );
        assert_eq!(
            normalize_edge_identity_kind("new_expression"),
            EDGE_IDENTITY_CONSTRUCTOR_LIKE
        );
        assert_eq!(
            normalize_edge_identity_kind("macro_invocation"),
            EDGE_IDENTITY_MACRO_LIKE
        );
        assert_eq!(
            normalize_edge_identity_kind("identifier"),
            EDGE_IDENTITY_BARE_IDENTIFIER
        );
    }
}
