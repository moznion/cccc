//! The compiled-in registry of supported languages.
//!
//! Each [`Language`] pairs a front-end adapter's `analyze_source`/`DEFAULT_EXTS`
//! with a user-facing name (and aliases). The unified `cccc` binary discovers a
//! file's language by its extension and dispatches to the matching adapter, so a
//! single run can analyze a mixed-language tree. Adding a language is now just
//! one entry here plus the adapter dependency — no new binary crate.

use std::collections::{BTreeMap, HashMap};

use crate::AnalyzeFn;

/// One supported language: how to name it on the command line, which file
/// extensions it claims, and how to analyze a file of that language.
pub struct Language {
    /// Canonical name used in `--lang` and diagnostics (e.g. `"rust"`).
    pub name: &'static str,
    /// Additional accepted spellings for `--lang` (e.g. `"rs"` for Rust).
    pub aliases: &'static [&'static str],
    /// File extensions (without the dot) this language analyzes by default.
    pub exts: &'static [&'static str],
    /// Lower+score a single file of this language.
    pub analyze: AnalyzeFn,
}

impl Language {
    /// True if `query` matches this language's canonical name or any alias
    /// (case-insensitive).
    fn matches(&self, query: &str) -> bool {
        self.name.eq_ignore_ascii_case(query)
            || self.aliases.iter().any(|a| a.eq_ignore_ascii_case(query))
    }
}

/// Every language compiled into the unified binary, in display order.
pub const LANGUAGES: &[Language] = &[
    Language {
        name: "es",
        aliases: &["ecmascript", "javascript", "typescript", "js", "ts"],
        exts: cccc_es::DEFAULT_EXTS,
        analyze: cccc_es::analyze_source,
    },
    Language {
        name: "rust",
        aliases: &["rs"],
        exts: cccc_rs::DEFAULT_EXTS,
        analyze: cccc_rs::analyze_source,
    },
    Language {
        name: "go",
        aliases: &["golang"],
        exts: cccc_go::DEFAULT_EXTS,
        analyze: cccc_go::analyze_source,
    },
    Language {
        name: "php",
        aliases: &[],
        exts: cccc_php::DEFAULT_EXTS,
        analyze: cccc_php::analyze_source,
    },
];

/// Resolve the active languages from an `include` (`--lang`) and an `exclude`
/// (`--exclude-lang`) filter.
///
/// The base set is the `include` list, or every registered language when
/// `include` is `None`. Any language named in `exclude` is then removed. Names
/// match a language's canonical name or an alias; an unrecognized name (in
/// either list) is an error, as is ending up with no languages at all.
pub fn resolve_languages(
    include: Option<&[String]>,
    exclude: Option<&[String]>,
) -> Result<Vec<&'static Language>, String> {
    let mut selected: Vec<&'static Language> = match include {
        Some(names) => {
            let mut v = Vec::new();
            for lang in resolve_each(names, "--lang")? {
                if !v.iter().any(|s: &&Language| s.name == lang.name) {
                    v.push(lang);
                }
            }
            v
        }
        None => LANGUAGES.iter().collect(),
    };

    if let Some(names) = exclude {
        let excluded = resolve_each(names, "--exclude-lang")?;
        selected.retain(|l| !excluded.iter().any(|e| e.name == l.name));
    }

    if selected.is_empty() {
        return Err("no languages selected after applying --lang/--exclude-lang".to_string());
    }
    Ok(selected)
}

/// Resolve every (non-empty) name in `names` to a [`Language`], erroring on the
/// first unrecognized one. `context` names the source for the error message.
fn resolve_each(names: &[String], context: &str) -> Result<Vec<&'static Language>, String> {
    let mut out = Vec::new();
    for name in names {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        match LANGUAGES.iter().find(|l| l.matches(name)) {
            Some(lang) => out.push(lang),
            None => return Err(unknown_language_error(name, context)),
        }
    }
    Ok(out)
}

/// The canonical name for the language named by `query` (its own name or an
/// alias), or `None` if no registered language matches.
pub fn canonical_name(query: &str) -> Option<&'static str> {
    LANGUAGES.iter().find(|l| l.matches(query)).map(|l| l.name)
}

/// Like [`canonical_name`] but returns a user-facing error (naming `context`,
/// e.g. `"--ext"` or `"[ext] config"`) when the language is unknown.
pub fn require_canonical(query: &str, context: &str) -> Result<&'static str, String> {
    canonical_name(query).ok_or_else(|| unknown_language_error(query, context))
}

/// Build the "unknown language '…' in <context> (known: …)" error message.
fn unknown_language_error(name: &str, context: &str) -> String {
    let known: Vec<&str> = LANGUAGES.iter().map(|l| l.name).collect();
    format!(
        "unknown language '{name}' in {context} (known: {})",
        known.join(", ")
    )
}

/// Build an extension → analyzer map for the given languages.
///
/// Each language contributes its [`Language::exts`], unless `ext_overrides`
/// supplies a replacement list keyed by that language's name or an alias (e.g.
/// `[ext]` with `es = ["ts"]`). Extensions are lowercased so lookup is
/// case-insensitive. Extensions are expected to be disjoint across languages;
/// if two languages claim the same one, the first (registration order) wins.
pub fn build_dispatch(
    langs: &[&'static Language],
    ext_overrides: &BTreeMap<String, Vec<String>>,
) -> HashMap<String, AnalyzeFn> {
    let mut map = HashMap::new();
    for lang in langs {
        let exts: Vec<String> = match override_for(lang, ext_overrides) {
            Some(list) => list.iter().map(|s| s.trim().to_ascii_lowercase()).collect(),
            None => lang.exts.iter().map(|s| s.to_ascii_lowercase()).collect(),
        };
        for ext in exts {
            map.entry(ext).or_insert(lang.analyze);
        }
    }
    map
}

/// The override extension list for `lang`, if any key in `overrides` names it.
fn override_for<'a>(
    lang: &Language,
    overrides: &'a BTreeMap<String, Vec<String>>,
) -> Option<&'a [String]> {
    overrides
        .iter()
        .find(|(key, _)| lang.matches(key))
        .map(|(_, list)| list.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(langs: &[&Language]) -> Vec<&'static str> {
        langs.iter().map(|l| l.name).collect()
    }

    #[test]
    fn no_filter_selects_all() {
        let langs = resolve_languages(None, None).unwrap();
        assert_eq!(langs.len(), LANGUAGES.len());
    }

    #[test]
    fn aliases_and_case_resolve() {
        let langs =
            resolve_languages(Some(&["RS".to_string(), "typescript".to_string()]), None).unwrap();
        assert_eq!(names(&langs), vec!["rust", "es"]);
    }

    #[test]
    fn duplicates_are_collapsed() {
        let langs =
            resolve_languages(Some(&["go".to_string(), "golang".to_string()]), None).unwrap();
        assert_eq!(langs.len(), 1);
    }

    #[test]
    fn unknown_language_errors() {
        assert!(resolve_languages(Some(&["cobol".to_string()]), None).is_err());
    }

    #[test]
    fn exclude_removes_from_all() {
        let langs = resolve_languages(None, Some(&["go".to_string(), "php".to_string()])).unwrap();
        assert_eq!(names(&langs), vec!["es", "rust"]);
    }

    #[test]
    fn exclude_narrows_an_include_list() {
        let langs = resolve_languages(
            Some(&["es".to_string(), "go".to_string(), "rust".to_string()]),
            Some(&["go".to_string()]),
        )
        .unwrap();
        assert_eq!(names(&langs), vec!["es", "rust"]);
    }

    #[test]
    fn exclude_accepts_aliases() {
        let langs = resolve_languages(None, Some(&["rs".to_string()])).unwrap();
        assert!(!names(&langs).contains(&"rust"));
    }

    #[test]
    fn excluding_everything_errors() {
        let all: Vec<String> = LANGUAGES.iter().map(|l| l.name.to_string()).collect();
        assert!(resolve_languages(None, Some(&all)).is_err());
    }

    #[test]
    fn unknown_excluded_language_errors() {
        assert!(resolve_languages(None, Some(&["cobol".to_string()])).is_err());
    }

    #[test]
    fn dispatch_covers_each_extension() {
        let all = resolve_languages(None, None).unwrap();
        let map = build_dispatch(&all, &BTreeMap::new());
        for key in ["ts", "rs", "go", "php"] {
            assert!(map.contains_key(key), "missing dispatch for .{key}");
        }
    }

    #[test]
    fn per_language_ext_override_replaces_defaults() {
        let all = resolve_languages(None, None).unwrap();
        let mut overrides = BTreeMap::new();
        // Restrict ES to .ts only, and give Go a custom extension.
        overrides.insert("es".to_string(), vec!["ts".to_string()]);
        overrides.insert("go".to_string(), vec!["go".to_string(), "tmpl".to_string()]);
        let map = build_dispatch(&all, &overrides);
        assert!(map.contains_key("ts"));
        assert!(!map.contains_key("tsx"), "tsx dropped by override");
        assert!(!map.contains_key("js"), "js dropped by override");
        assert!(map.contains_key("tmpl"), "custom Go extension routed");
    }

    #[test]
    fn ext_override_accepts_aliases() {
        let all = resolve_languages(None, None).unwrap();
        let mut overrides = BTreeMap::new();
        overrides.insert("typescript".to_string(), vec!["ts".to_string()]);
        let map = build_dispatch(&all, &overrides);
        assert!(map.contains_key("ts") && !map.contains_key("tsx"));
    }

    #[test]
    fn canonical_name_resolves_aliases() {
        assert_eq!(canonical_name("ts"), Some("es"));
        assert_eq!(canonical_name("RUST"), Some("rust"));
        assert_eq!(canonical_name("cobol"), None);
    }

    #[test]
    fn require_canonical_rejects_unknown() {
        assert!(require_canonical("cobol", "--ext").is_err());
    }
}
