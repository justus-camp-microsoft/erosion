pub mod gleam;
pub mod javascript;
pub mod python;
pub mod rust;
mod tree;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::metrics::FileAnalysis;

pub const PARSER_VERSIONS: &str = "tree-sitter=0.25.2;javascript=0.25.0;typescript=0.23.2-erosion.2;python=0.25.0;rust=0.24.2-erosion.1;gleam=git-cefbd686";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    JavaScript,
    TypeScript,
    Tsx,
    Python,
    Rust,
    Gleam,
}

impl Language {
    pub const ALL: [Self; 6] = [
        Self::JavaScript,
        Self::TypeScript,
        Self::Tsx,
        Self::Python,
        Self::Rust,
        Self::Gleam,
    ];

    pub fn for_path(path: &str) -> Option<Self> {
        match Path::new(path)
            .extension()?
            .to_str()?
            .to_ascii_lowercase()
            .as_str()
        {
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "py" | "pyw" | "pyi" => Some(Self::Python),
            "rs" => Some(Self::Rust),
            "gleam" => Some(Self::Gleam),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Gleam => "gleam",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LanguageTestPolicy {
    pub version: &'static str,
    pub path_patterns: &'static [&'static str],
    pub syntax_rules: &'static [&'static str],
}

#[derive(Clone, Debug, Serialize)]
pub struct TestExclusionPolicy {
    pub version: &'static str,
    pub languages: BTreeMap<String, LanguageTestPolicy>,
}

pub trait LanguageAdapter {
    fn test_policy(&self) -> LanguageTestPolicy;
    fn is_test_file(&self, path: &str) -> bool;
    fn analyze(&mut self, source: &[u8]) -> Result<FileAnalysis>;
}

pub fn adapter(language: Language) -> Result<Box<dyn LanguageAdapter>> {
    match language {
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            Ok(Box::new(javascript::JavaScriptAdapter::new(language)?))
        }
        Language::Python => Ok(Box::new(python::PythonAdapter::new()?)),
        Language::Rust => Ok(Box::new(rust::RustAdapter::new()?)),
        Language::Gleam => Ok(Box::new(gleam::GleamAdapter::new()?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_routes_each_language_to_its_grammar() {
        for (language, source) in [
            (Language::JavaScript, "const view = () => <div />;"),
            (
                Language::TypeScript,
                "const value = (x: number): number => x;",
            ),
            (Language::Tsx, "const view = (x: string) => <div>{x}</div>;"),
            (Language::Python, "def value(x):\n    return x\n"),
            (Language::Rust, "fn value(x: i32) -> i32 { x }"),
            (Language::Gleam, "pub fn value(x: Int) -> Int { x }"),
        ] {
            let analysis = adapter(language)
                .unwrap()
                .analyze(source.as_bytes())
                .unwrap();
            assert!(
                analysis.parsed(),
                "{language:?}: {:?}",
                analysis.diagnostics
            );
            assert_eq!(analysis.functions.len(), 1, "{language:?}");
            assert_eq!(analysis.functions[0].complexity, 1, "{language:?}");
        }
    }

    #[test]
    fn javascript_adapter_rejects_other_languages() {
        for language in [Language::Python, Language::Rust, Language::Gleam] {
            let error = javascript::JavaScriptAdapter::new(language)
                .err()
                .expect("Non-JavaScript languages must be rejected");
            assert_eq!(
                error.to_string(),
                format!("JavaScript adapter does not support {}", language.id())
            );
        }
    }
}
