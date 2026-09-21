#!/usr/bin/env -S rust-script --force
//! Parse Detcore library source and report references to non-core Reverie crates.
//!
//! ```cargo
//! [dependencies]
//! proc-macro2 = { version = "1", features = ["span-locations"] }
//! syn = { version = "2", features = ["full", "visit"] }
//! ```

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use proc_macro2::Span;
use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use syn::ItemExternCrate;
use syn::ItemUse;
use syn::UseTree;
use syn::visit;
use syn::visit::Visit;

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

fn usage() {
    println!("usage: detcore-backend-source.rs SOURCE_ROOT MODULE [MODULE ...]");
}

fn rust_sources(root: &Path) -> Result<Vec<PathBuf>, String> {
    fn visit(directory: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in fs::read_dir(directory)
            .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        {
            let entry = entry.map_err(|error| {
                format!("cannot read entry under {}: {error}", directory.display())
            })?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot classify {}: {error}", path.display()))?;
            if file_type.is_dir() {
                visit(&path, output)?;
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                output.push(path);
            }
        }
        Ok(())
    }

    let mut paths = Vec::new();
    visit(root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

struct BackendReferenceVisitor<'a> {
    prohibited: &'a BTreeSet<String>,
    hits: BTreeSet<(usize, String)>,
}

impl BackendReferenceVisitor<'_> {
    fn record(&mut self, module: &str, span: Span) {
        if self.prohibited.contains(module) {
            self.hits.insert((span.start().line, module.to_owned()));
        }
    }

    fn record_use_root(&mut self, tree: &UseTree) {
        match tree {
            UseTree::Path(path) => self.record(&path.ident.to_string(), path.ident.span()),
            UseTree::Name(name) => self.record(&name.ident.to_string(), name.ident.span()),
            UseTree::Rename(rename) => self.record(&rename.ident.to_string(), rename.ident.span()),
            UseTree::Group(group) => {
                for item in &group.items {
                    self.record_use_root(item);
                }
            }
            UseTree::Glob(_) => {}
        }
    }

    fn record_token_paths(&mut self, stream: TokenStream) {
        let tokens = stream.into_iter().collect::<Vec<_>>();
        for (index, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                self.record_token_paths(group.stream());
            }
            let TokenTree::Ident(ident) = token else {
                continue;
            };
            let is_path = matches!(tokens.get(index + 1), Some(TokenTree::Punct(first)) if first.as_char() == ':')
                && matches!(tokens.get(index + 2), Some(TokenTree::Punct(second)) if second.as_char() == ':');
            if is_path {
                self.record(&ident.to_string(), ident.span());
            }
        }
    }
}

impl<'ast> Visit<'ast> for BackendReferenceVisitor<'_> {
    fn visit_item_extern_crate(&mut self, node: &'ast ItemExternCrate) {
        self.record(&node.ident.to_string(), node.ident.span());
        visit::visit_item_extern_crate(self, node);
    }

    fn visit_item_use(&mut self, node: &'ast ItemUse) {
        self.record_use_root(&node.tree);
        visit::visit_item_use(self, node);
    }

    fn visit_path(&mut self, node: &'ast syn::Path) {
        if let Some(first) = node.segments.first() {
            self.record(&first.ident.to_string(), first.ident.span());
        }
        visit::visit_path(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.record_token_paths(node.tokens.clone());
        visit::visit_macro(self, node);
    }
}

fn run() -> Result<bool, String> {
    let mut args = env::args().skip(1);
    let Some(first) = args.next() else {
        usage();
        return Err("missing source root".into());
    };
    if first == "--help" || first == "-h" {
        usage();
        return Ok(false);
    }
    let source_root = PathBuf::from(first);
    if !source_root.is_dir() {
        return Err(format!(
            "Detcore source root is not a directory: {}",
            source_root.display()
        ));
    }
    let prohibited = args.collect::<BTreeSet<_>>();
    if prohibited.is_empty() {
        return Err("no prohibited Reverie modules were supplied".into());
    }

    let mut found = false;
    for path in rust_sources(&source_root)? {
        let source = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let syntax = syn::parse_file(&source)
            .map_err(|error| format!("cannot parse {} as Rust: {error}", path.display()))?;
        let mut visitor = BackendReferenceVisitor {
            prohibited: &prohibited,
            hits: BTreeSet::new(),
        };
        visitor.visit_file(&syntax);
        let lines = source.lines().collect::<Vec<_>>();
        for (line, _module) in visitor.hits {
            let text = lines.get(line.saturating_sub(1)).copied().unwrap_or("");
            println!("{}:{line}:{}", path.display(), text.trim());
            found = true;
        }
    }
    Ok(found)
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("detcore-backend-source: {error}");
            ExitCode::from(2)
        }
    }
}
