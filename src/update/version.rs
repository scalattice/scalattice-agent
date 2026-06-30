use std::cmp::Ordering;

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn normalize_version(raw: &str) -> String {
    raw.trim().trim_start_matches('v').trim().to_string()
}

pub fn parse_version(raw: &str) -> Option<(u32, u32, u32)> {
    let normalized = normalize_version(raw);
    let mut parts = normalized.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

pub fn compare_versions(latest: &str, current: &str) -> Ordering {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l.cmp(&c),
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_version() {
        assert_eq!(
            compare_versions("1.0.33", "1.0.32"),
            Ordering::Greater
        );
    }

    #[test]
    fn same_with_v_prefix() {
        assert_eq!(
            compare_versions("v1.0.32", "1.0.32"),
            Ordering::Equal
        );
    }
}
