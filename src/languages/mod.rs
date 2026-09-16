pub mod gleam;
pub mod javascript;
pub mod python;
pub mod rust;
mod tree;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::metrics::FileAnalysis;

pub const PARSER_VERSIONS: &str =
    "tree-sitter=0.25.2;javascript=0.25.0;typescript=0.23.2;python=0.25.0;rust=0.24.2;gleam=git-cefbd686";

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

pub trait LanguageAdapter {
    fn analyze(&mut self, source: &[u8]) -> Result<FileAnalysis>;
}

pub fn adapter(language: Language) -> Result<Box<dyn LanguageAdapter>> {
    match language {
        Language::Python => Ok(Box::new(python::PythonAdapter::new()?)),
        Language::Rust => Ok(Box::new(rust::RustAdapter::new()?)),
        Language::Gleam => Ok(Box::new(gleam::GleamAdapter::new()?)),
        _ => Ok(Box::new(javascript::JavaScriptAdapter::new(language)?)),
    }
}
