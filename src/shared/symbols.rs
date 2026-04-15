pub const EDGE_IDENTITY_BARE_IDENTIFIER: &str = "bare_identifier";
pub const EDGE_IDENTITY_QUALIFIED_PATH: &str = "qualified_path";
pub const EDGE_IDENTITY_MEMBER_ACCESS: &str = "member_access";
pub const EDGE_IDENTITY_METHOD_RECEIVER: &str = "method_receiver";
pub const EDGE_IDENTITY_CONSTRUCTOR_LIKE: &str = "constructor_like";
pub const EDGE_IDENTITY_MACRO_LIKE: &str = "macro_like";

#[allow(dead_code)]
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

#[allow(dead_code)]
pub fn split_symbol_components(value: &str) -> Vec<String> {
    value
        .split(is_symbol_component_separator)
        .map(normalize_symbolish)
        .filter(|component| !component.is_empty())
        .collect()
}

#[allow(dead_code)]
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

#[allow(dead_code)]
pub fn owner_fingerprint(value: &str) -> String {
    owner_fingerprint_from_components(&split_symbol_components(value))
}

#[allow(dead_code)]
pub fn owner_fingerprint_from_components(components: &[String]) -> String {
    owner_components(components).join("/")
}

#[allow(dead_code)]
pub fn owner_components_share_suffix(left: &[String], right: &[String]) -> bool {
    let left = owner_components(left);
    let right = owner_components(right);
    let shared_len = left
        .iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(lhs, rhs)| lhs == rhs)
        .count();
    shared_len > 0
}

#[allow(dead_code)]
pub fn owner_components_share_subsequence(left: &[String], right: &[String]) -> bool {
    let left = owner_components(left);
    let right = owner_components(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }

    let (needle, haystack) = if left.len() <= right.len() {
        (left, right)
    } else {
        (right, left)
    };

    let mut haystack_iter = haystack.iter();
    needle
        .iter()
        .all(|needle_component| haystack_iter.any(|candidate| candidate == needle_component))
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
        owner_components_share_subsequence, owner_components_share_suffix, owner_fingerprint,
        owner_fingerprint_from_components, split_symbol_components, EDGE_IDENTITY_BARE_IDENTIFIER,
        EDGE_IDENTITY_CONSTRUCTOR_LIKE, EDGE_IDENTITY_MACRO_LIKE, EDGE_IDENTITY_MEMBER_ACCESS,
        EDGE_IDENTITY_METHOD_RECEIVER, EDGE_IDENTITY_QUALIFIED_PATH,
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
            owner_fingerprint("crate::auth::config::load_config"),
            "auth/config"
        );
        assert_eq!(
            owner_fingerprint_from_components(&split_symbol_components("src/search/impact.rs")),
            "search"
        );
    }

    #[test]
    fn compares_owner_components_by_suffix_and_subsequence() {
        let qualified = split_symbol_components("crate::auth::config::load_config");
        let file_path = split_symbol_components("src/internal/auth/config/loader.rs");
        let subsequence = split_symbol_components("repo/auth/runtime/config/loader.rs");

        assert!(owner_components_share_suffix(&qualified, &file_path));
        assert!(owner_components_share_subsequence(&qualified, &subsequence));
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
