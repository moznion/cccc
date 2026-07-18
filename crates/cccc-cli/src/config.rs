//! Optional `cccc.toml` configuration file.
//!
//! A config file lets a project bake in the options it normally runs with
//! (extensions, excludes, thresholds, …) so they don't have to be repeated on
//! every invocation. Resolution precedence is **CLI flag > config file >
//! built-in default**: anything given explicitly on the command line always
//! wins, the config file fills in the rest, and built-in defaults apply where
//! neither speaks.
//!
//! By default the file is discovered by walking up from the current directory,
//! looking for `cccc.toml` (then `.cccc.toml`) in each ancestor — so running
//! `cccc` from anywhere inside a project picks up the project's config.
//! `--config <path>` points at an explicit file (which must exist) and
//! `--no-config` disables discovery entirely.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// File names searched for during upward discovery, in priority order.
const CONFIG_NAMES: &[&str] = &["cccc.toml", ".cccc.toml"];

/// The deserialized `cccc.toml`. Every field is optional; an absent field means
/// "defer to the CLI flag or the built-in default". Unknown keys are rejected so
/// typos surface as errors instead of being silently ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// Print a human-readable table instead of JSON.
    pub table: Option<bool>,
    /// Per-language file-extension overrides, e.g. `[ext]` with `es = ["ts"]`.
    /// Each entry replaces that language's default extensions (and so also
    /// routes those extensions to it). Keyed by a language's canonical name or
    /// an alias. Languages without an entry keep their defaults.
    pub ext: Option<BTreeMap<String, Vec<String>>>,
    /// Glob patterns of files to exclude.
    pub exclude: Option<Vec<String>>,
    /// Languages to analyze (canonical names or aliases).
    pub languages: Option<Vec<String>>,
    /// Languages to exclude from analysis (the inverse of `languages`).
    pub exclude_languages: Option<Vec<String>>,
    /// Fail if any function's cognitive complexity exceeds this.
    pub max_cognitive: Option<u32>,
    /// Fail if any function's cyclomatic complexity exceeds this.
    pub max_cyclomatic: Option<u32>,
    /// Only report functions whose complexity is >= this.
    pub min: Option<u32>,
    /// Do not respect .gitignore / ignore files.
    pub no_ignore: Option<bool>,
    /// Number of files to analyze in parallel.
    pub jobs: Option<u32>,
    /// Pretty-print the JSON output (the default is compact, one line).
    pub pretty: Option<bool>,
    /// Cache results and reuse them for files unchanged since the last run.
    pub cache: Option<bool>,
    /// Where to keep the results cache (default: `.cccc.cache` next to this
    /// config file). Does not enable caching by itself.
    pub cache_file: Option<PathBuf>,
    /// Directory containing the loaded config file. Not a config key — filled
    /// in at load time so the default cache location can sit beside the file.
    #[serde(skip)]
    pub source_dir: Option<PathBuf>,
}

impl Config {
    /// Resolve the config for this run, honoring `--config` / `--no-config`.
    ///
    /// - `--no-config` (`no_config = true`): always returns an empty config.
    /// - `--config <path>` (`explicit = Some`): loads exactly that file; a
    ///   missing or malformed file is an error.
    /// - neither: discovers a file by walking up from the current directory,
    ///   returning the empty config if none is found.
    pub fn resolve(explicit: Option<&Path>, no_config: bool) -> Result<Config, String> {
        if no_config {
            return Ok(Config::default());
        }
        if let Some(path) = explicit {
            if !path.exists() {
                return Err(format!("config file not found: {}", path.display()));
            }
            return load(path);
        }
        match find_config() {
            Some(path) => load(&path),
            None => Ok(Config::default()),
        }
    }
}

/// Walk up from the current directory looking for a config file. Returns the
/// path of the first match (checking [`CONFIG_NAMES`] in order within each
/// directory), or `None` if none exists up to the filesystem root.
fn find_config() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    for dir in cwd.ancestors() {
        for name in CONFIG_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Read and parse a config file, turning IO/parse failures into messages.
fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
    let mut config: Config =
        toml::from_str(&text).map_err(|e| format!("invalid config {}: {e}", path.display()))?;
    config.source_dir = path.parent().map(Path::to_path_buf);
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_fields() {
        let cfg: Config = toml::from_str(
            r#"
            table = true
            exclude = ["dist/**"]
            languages = ["es", "go"]
            max-cognitive = 15
            max-cyclomatic = 10
            min = 1
            no-ignore = true
            jobs = 8

            [ext]
            es = ["ts", "tsx"]
            go = ["go", "tmpl"]
        "#,
        )
        .unwrap();
        assert_eq!(cfg.table, Some(true));
        let ext = cfg.ext.as_ref().unwrap();
        assert_eq!(ext["es"], vec!["ts".to_string(), "tsx".to_string()]);
        assert_eq!(ext["go"], vec!["go".to_string(), "tmpl".to_string()]);
        assert_eq!(
            cfg.languages.as_deref(),
            Some(&["es".to_string(), "go".to_string()][..])
        );
        assert_eq!(cfg.max_cognitive, Some(15));
        assert_eq!(cfg.jobs, Some(8));
    }

    #[test]
    fn empty_config_is_all_none() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.table.is_none() && cfg.ext.is_none() && cfg.max_cognitive.is_none());
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(toml::from_str::<Config>("nonsense = 1").is_err());
    }

    #[test]
    fn no_config_yields_default() {
        let cfg = Config::resolve(None, true).unwrap();
        assert!(cfg.ext.is_none());
    }

    #[test]
    fn explicit_missing_config_errors() {
        let err = Config::resolve(Some(Path::new("/no/such/cccc.toml")), false).unwrap_err();
        assert!(err.contains("not found"));
    }
}
