/// Whether one argument requests conventional command-line help.
///
/// The manifest tools deliberately keep their small hand-written parsers. This
/// predicate gives every binary in the package one spelling authority without
/// adding a parsing dependency or changing any non-help argument semantics.
pub fn is_help_flag(argument: &str) -> bool {
    matches!(argument, "-h" | "--help")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_conventional_help_flags_match() {
        assert!(is_help_flag("-h"));
        assert!(is_help_flag("--help"));
        assert!(!is_help_flag("help"));
        assert!(!is_help_flag("--format"));
        assert!(!is_help_flag(""));
    }
}
