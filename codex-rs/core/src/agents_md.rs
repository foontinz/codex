/// Default filename scanned for project-level docs.
pub(crate) const DEFAULT_PROJECT_DOC_FILENAME: &str = "AGENTS.md";
/// Preferred local override for project-level docs.
pub(crate) const LOCAL_PROJECT_DOC_FILENAME: &str = "AGENTS.override.md";

pub(crate) fn filename_priority(file_name: &str) -> Option<u8> {
    match file_name {
        LOCAL_PROJECT_DOC_FILENAME => Some(0),
        DEFAULT_PROJECT_DOC_FILENAME => Some(1),
        _ => None,
    }
}

pub(crate) fn candidate_filenames<'a>(fallback_filenames: &'a [String]) -> Vec<&'a str> {
    let mut names: Vec<&'a str> = Vec::with_capacity(2 + fallback_filenames.len());
    names.push(LOCAL_PROJECT_DOC_FILENAME);
    names.push(DEFAULT_PROJECT_DOC_FILENAME);
    for candidate in fallback_filenames {
        let candidate = candidate.as_str();
        if candidate.is_empty() || names.contains(&candidate) {
            continue;
        }
        names.push(candidate);
    }
    names
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    #[test]
    fn filename_priority_prefers_override() {
        assert_eq!(super::filename_priority("AGENTS.override.md"), Some(0));
        assert_eq!(super::filename_priority("AGENTS.md"), Some(1));
        assert_eq!(super::filename_priority("OTHER.md"), None);
    }

    #[test]
    fn candidate_filenames_include_defaults_then_unique_fallbacks() {
        let fallbacks = vec![
            "".to_string(),
            "AGENTS.md".to_string(),
            "CUSTOM.md".to_string(),
            "CUSTOM.md".to_string(),
        ];
        assert_eq!(
            super::candidate_filenames(&fallbacks),
            vec!["AGENTS.override.md", "AGENTS.md", "CUSTOM.md"],
        );
    }
}
