//! Symbol names: demangling and the matching rules for function lookup by name.

/// Demangle a Rust (legacy or v0) symbol without its hash; other names come back unchanged.
pub fn demangle(raw: &str) -> String {
    match rustc_demangle::try_demangle(raw) {
        Ok(d) => format!("{d:#}"),
        Err(_) => raw.to_string(),
    }
}

/// `name` with every generic argument list removed: `std::rt::lang_start<()>` becomes
/// `std::rt::lang_start`, `<T as Tr>::f` is left as written apart from inner generics.
pub fn strip_generics(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0usize;
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                // A leading `<` (qualified path such as `<T as Tr>::f`) is kept.
                if depth == 0 && (out.is_empty() || out.ends_with("::")) {
                    out.push(c);
                    depth = 0;
                    // Copy the qualified-path segment verbatim up to its matching `>`.
                    let mut inner = 1usize;
                    for c2 in chars.by_ref() {
                        out.push(c2);
                        match c2 {
                            '<' => inner += 1,
                            '>' => {
                                inner -= 1;
                                if inner == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                } else {
                    depth += 1;
                }
            }
            '>' if depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            _ => out.push(c),
        }
    }
    out
}

/// Whether `name` (a `::`-separated path) ends with `query` on a path-segment boundary:
/// `worked_example::load` matches `load` and `worked_example::load`, not `reload`.
pub fn path_suffix_match(name: &str, query: &str) -> bool {
    if name == query {
        return true;
    }
    name.len() > query.len()
        && name.ends_with(query)
        && name[..name.len() - query.len()].ends_with("::")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demangles_v0_and_legacy() {
        assert_eq!(demangle("_RNvCsk2gAxyzTTfV_6values4main"), "values::main");
        assert_eq!(demangle("_ZN6values9stop_here17h0123456789abcdefE"), "values::stop_here");
        assert_eq!(demangle("main"), "main");
    }

    #[test]
    fn strips_generics() {
        assert_eq!(strip_generics("std::rt::lang_start<()>"), "std::rt::lang_start");
        assert_eq!(
            strip_generics("core::ops::function::FnOnce::call_once<fn(), ()>"),
            "core::ops::function::FnOnce::call_once"
        );
        assert_eq!(
            strip_generics("<alloc::string::String as core::clone::Clone>::clone"),
            "<alloc::string::String as core::clone::Clone>::clone"
        );
        assert_eq!(strip_generics("a::b<c<d>>::e"), "a::b::e");
    }

    #[test]
    fn suffix_on_segments() {
        assert!(path_suffix_match("worked_example::load", "load"));
        assert!(path_suffix_match("worked_example::load", "worked_example::load"));
        assert!(!path_suffix_match("worked_example::reload", "load"));
        assert!(!path_suffix_match("load", "worked_example::load"));
    }
}
