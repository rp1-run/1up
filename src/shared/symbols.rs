pub fn normalize_symbolish(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::normalize_symbolish;

    #[test]
    fn normalizes_symbolish_queries() {
        assert_eq!(normalize_symbolish("ConfigLoader"), "configloader");
        assert_eq!(normalize_symbolish("config_loader"), "configloader");
        assert_eq!(normalize_symbolish("Config Loader"), "configloader");
    }
}
