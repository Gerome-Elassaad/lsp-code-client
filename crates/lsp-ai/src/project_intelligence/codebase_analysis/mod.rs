pub mod parser;

use parser::{RustCodeParser, JavascriptCodeParser};
use crate::config::Config;
use tracing::info;

pub struct CodebaseAnalyzer {
    config: Config,
}

impl CodebaseAnalyzer {
    pub fn new(config: Config) -> Self {
        CodebaseAnalyzer {
            config,
        }
    }

    pub fn analyze_code(&self, code: &str, language: &str) -> Option<tree_sitter::Tree> {
        info!("analyze_code called with code: {:?} and language: {:?}", code, language);
        match language {
            "rust" => {
                let parser = RustCodeParser::new();
                let function_names = parser.extract_function_definitions(code);
                println!("Function names: {:?}", function_names);
                let tree = parser.parse(code);
                tree
            }
            "javascript" => {
                let parser = JavascriptCodeParser::new();
                parser.parse(code)
            }
            _ => None,
        }
    }
}
