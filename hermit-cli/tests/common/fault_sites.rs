//! Structural fault-site discovery for the CLI's actual container calls.
//! Only literal strings and conditional literal branches are supported labels.
//! Local identifiers fail closed; comments and string contents are never calls.
//! This is a direct-AST/token audit, not Rust name resolution or macro expansion.
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use proc_macro2::TokenStream;
use proc_macro2::TokenTree;
use quote::ToTokens;
use syn::Expr;
use syn::visit::Visit;

fn test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        path_is(attribute.path(), "cfg")
            && attribute
                .parse_args::<syn::Path>()
                .is_ok_and(|path| path_is(&path, "test"))
    })
}

fn call_path(expr: &Expr) -> Option<String> {
    let Expr::Path(path) = expr else { return None };
    if path.qself.is_some()
        || path
            .path
            .segments
            .iter()
            .any(|s| !matches!(s.arguments, syn::PathArguments::None))
    {
        return None;
    }
    Some(
        path.path
            .segments
            .iter()
            .map(|s| ident_name(&s.ident))
            .collect::<Vec<_>>()
            .join("::"),
    )
}

fn local_name(expr: &Expr) -> Option<String> {
    let Expr::Path(path) = expr else { return None };
    (path.attrs.is_empty() && path.qself.is_none())
        .then(|| path.path.get_ident())
        .flatten()
        .map(ident_name)
}

fn binding_free_condition(expr: &Expr) -> bool {
    #[derive(Default)]
    struct Condition {
        unsupported: bool,
    }
    impl<'ast> Visit<'ast> for Condition {
        fn visit_pat(&mut self, _: &'ast syn::Pat) {
            self.unsupported = true;
        }

        fn visit_expr(&mut self, expression: &'ast Expr) {
            if matches!(expression, Expr::Macro(_) | Expr::Verbatim(_)) {
                self.unsupported = true;
            } else {
                syn::visit::visit_expr(self, expression);
            }
        }
    }
    let mut condition = Condition::default();
    condition.visit_expr(expr);
    !condition.unsupported
}

// Reconcile exact source locations, not totals: an accounted token cannot cancel
// a different hidden occurrence in a macro, import, method or function value.
type TokenKey = (String, usize, usize);
fn critical(name: &str) -> bool {
    matches!(
        name.trim_start_matches("r#"),
        "owned_container" | "catch_child_panic_at" | "inject_test_fault" | "run_guarded_at"
    )
}
fn token_key(ident: &proc_macro2::Ident) -> TokenKey {
    let at = ident.span().start();
    (ident.to_string(), at.line, at.column)
}
fn ident_name(ident: &proc_macro2::Ident) -> String {
    ident.to_string().trim_start_matches("r#").into()
}
fn ident_is(ident: &proc_macro2::Ident, name: &str) -> bool {
    ident_name(ident) == name
}
fn path_is(path: &syn::Path, name: &str) -> bool {
    path.get_ident().is_some_and(|ident| ident_is(ident, name))
}
fn critical_tokens(tokens: TokenStream) -> BTreeSet<TokenKey> {
    let mut found = BTreeSet::new();
    for token in tokens {
        match token {
            TokenTree::Ident(ident) if critical(&ident.to_string()) => {
                found.insert(token_key(&ident));
            }
            TokenTree::Group(group) => found.extend(critical_tokens(group.stream())),
            _ => {}
        }
    }
    found
}

fn contains_ident(tokens: TokenStream, name: &str) -> bool {
    tokens.into_iter().any(|token| match token {
        TokenTree::Ident(ident) => ident_is(&ident, name),
        TokenTree::Group(group) => contains_ident(group.stream(), name),
        _ => false,
    })
}

fn non_ascii_tokens(tokens: TokenStream) -> bool {
    tokens.into_iter().any(|token| match token {
        TokenTree::Ident(ident) => !ident_name(&ident).is_ascii(),
        TokenTree::Group(group) => non_ascii_tokens(group.stream()),
        _ => false,
    })
}

#[derive(Default)]
struct Sites {
    file: String,
    accounted: BTreeSet<TokenKey>,
    function_depth: usize,
    module_depth: usize,
    forwarding: Option<&'static str>,
    owner_proven: bool,
    boundary_proven: bool,
    declarations: BTreeSet<TokenKey>,
    labels: Vec<String>,
    error: Option<String>,
}

impl Sites {
    fn values(&self, expr: &Expr) -> Result<Vec<String>, String> {
        match expr {
            Expr::Lit(lit) if lit.attrs.is_empty() => match &lit.lit {
                syn::Lit::Str(value) => Ok(vec![value.value()]),
                _ => Err("site is not a string literal".into()),
            },
            Expr::Block(block) if block.attrs.is_empty() => self.block_values(&block.block),
            Expr::If(branch) if branch.attrs.is_empty() => {
                if !binding_free_condition(&branch.cond) {
                    return Err("binding-bearing or opaque site condition is unsupported".into());
                }
                let mut values = self.block_values(&branch.then_branch)?;
                let (_, otherwise) = branch.else_branch.as_ref().ok_or("site if lacks else")?;
                values.extend(self.values(otherwise)?);
                Ok(values)
            }
            Expr::Paren(inner) if inner.attrs.is_empty() => self.values(&inner.expr),
            _ => Err("unsupported site expression: use literal strings or conditional literal branches, never local identifiers".into()),
        }
    }

    fn block_values(&self, block: &syn::Block) -> Result<Vec<String>, String> {
        match block.stmts.as_slice() {
            [syn::Stmt::Expr(expr, None)] => self.values(expr),
            _ => Err("site branch must contain one value expression".into()),
        }
    }
}

impl<'ast> Visit<'ast> for Sites {
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if test_only(&function.attrs) {
            self.accounted
                .extend(critical_tokens(function.to_token_stream()));
            return;
        }
        let top = self.function_depth == 0 && self.module_depth == 0;
        let name = ident_name(&function.sig.ident);
        let previous = self.forwarding;
        self.forwarding =
            if top && self.file == "owned_container.rs" && name == "run" && self.owner_proven {
                Some("super::container::catch_child_panic_at")
            } else if top
                && self.file == "container.rs"
                && name == "catch_child_panic_at"
                && self.boundary_proven
            {
                self.accounted.insert(token_key(&function.sig.ident));
                Some("inject_test_fault")
            } else {
                None
            };
        if top && self.declarations.contains(&token_key(&function.sig.ident)) {
            self.accounted.insert(token_key(&function.sig.ident));
        }
        self.function_depth += 1;
        syn::visit::visit_item_fn(self, function);
        self.function_depth -= 1;
        self.forwarding = previous;
    }

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if test_only(&module.attrs) {
            self.accounted
                .extend(critical_tokens(module.to_token_stream()));
            return;
        }
        if self.module_depth == 0
            && self.function_depth == 0
            && self.declarations.contains(&token_key(&module.ident))
        {
            self.accounted.insert(token_key(&module.ident));
        }
        self.module_depth += 1;
        syn::visit::visit_item_mod(self, module);
        self.module_depth -= 1;
    }

    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        if path_is(attribute.path(), "path")
            || (path_is(attribute.path(), "cfg_attr")
                && contains_ident(attribute.meta.to_token_stream(), "path"))
        {
            self.error = Some("source path indirection is unsupported".into());
        }
        syn::visit::visit_attribute(self, attribute);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac
            .path
            .segments
            .last()
            .is_some_and(|part| ident_is(&part.ident, "include"))
        {
            self.error = Some("source include indirection is unsupported".into());
        }
        syn::visit::visit_macro(self, mac);
    }

    fn visit_type_path(&mut self, ty: &'ast syn::TypePath) {
        // These are cleanup error downcast types, not executable site calls.
        // Account just the exact module token in this AST type, never an entire
        // expression/import or an arbitrary use of the module name.
        if ty.qself.is_none()
            && ty.path.leading_colon.is_none()
            && ty
                .path
                .segments
                .iter()
                .all(|part| matches!(part.arguments, syn::PathArguments::None))
            && ty
                .path
                .segments
                .iter()
                .map(|part| ident_name(&part.ident))
                .collect::<Vec<_>>()
                == ["super", "owned_container", "ParentCleanupUnconfirmed"]
        {
            self.accounted.insert(token_key(&ty.path.segments[1].ident));
        }
        syn::visit::visit_type_path(self, ty);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let path = call_path(&call.func).unwrap_or_default();
        let index = if path == "owned_container::run" || path.ends_with("::owned_container::run") {
            Some(4)
        } else if matches!(
            path.rsplit("::").next(),
            Some("run_guarded_at" | "inject_test_fault" | "catch_child_panic_at")
        ) {
            Some(0)
        } else {
            None
        };
        if let Some(index) = index {
            let argument = call.args.iter().nth(index);
            // The separately checked boundary forwards its parameter. Its
            // callers, rather than a made-up literal here, supply the labels.
            if let Expr::Path(function) = call.func.as_ref() {
                // Only the callee's critical segment is accounted. Critical
                // identifiers elsewhere in the path/arguments remain visible.
                let segment = if index == 4 {
                    function.path.segments.iter().rev().nth(1).unwrap()
                } else {
                    function.path.segments.last().unwrap()
                };
                self.accounted.insert(token_key(&segment.ident));
            }
            let forwarded = self.forwarding == Some(path.as_str())
                && argument.and_then(local_name).as_deref() == Some("site");
            if !forwarded {
                match argument
                    .ok_or_else(|| format!("missing site argument in {path}"))
                    .and_then(|value| self.values(value))
                {
                    Ok(labels) => self.labels.extend(labels),
                    Err(error) => {
                        self.error.get_or_insert_with(|| format!("{path}: {error}"));
                    }
                }
            }
        }
        syn::visit::visit_expr_call(self, call);
    }
}

fn inspect(file_name: &str, text: &str) -> Result<Sites, String> {
    let file = syn::parse_file(text).map_err(|error| error.to_string())?;
    let all = critical_tokens(
        text.parse()
            .map_err(|error| format!("tokenization: {error}"))?,
    );
    let mut sites = Sites {
        file: file_name.into(),
        owner_proven: file_name == "owned_container.rs"
            && forwards_parameter(text, "run", 4, "super::container::catch_child_panic_at"),
        boundary_proven: file_name == "container.rs"
            && forwards_parameter(text, "catch_child_panic_at", 0, "inject_test_fault"),
        ..Sites::default()
    };
    // Declaration exclusions bind exact, unique top-level AST nodes. An extra
    // same-named declaration cannot borrow the real function's forwarding proof.
    if file_name == "container.rs" {
        let declarations = file
            .items
            .iter()
            .filter_map(|item| match item {
                syn::Item::Fn(item) if ident_is(&item.sig.ident, "inject_test_fault") => Some(item),
                _ => None,
            })
            .collect::<Vec<_>>();
        if let [function] = declarations.as_slice()
            && function.sig.inputs.len() == 1
            && matches!(function.sig.inputs.first(), Some(syn::FnArg::Typed(arg)) if matches!(arg.pat.as_ref(), syn::Pat::Ident(p) if ident_is(&p.ident, "site") && p.mutability.is_none() && p.by_ref.is_none() && p.subpat.is_none()))
        {
            sites.declarations.insert(token_key(&function.sig.ident));
        }
    }
    if file_name == "main.rs" {
        let declarations = file
            .items
            .iter()
            .filter_map(|item| match item {
                syn::Item::Mod(item) if ident_is(&item.ident, "owned_container") => Some(item),
                _ => None,
            })
            .collect::<Vec<_>>();
        if let [module] = declarations.as_slice()
            && module.content.is_none()
            && module.attrs.is_empty()
            && matches!(module.vis, syn::Visibility::Inherited)
        {
            sites.declarations.insert(token_key(&module.ident));
        }
    }
    sites.visit_file(&file);
    let unaccounted = all.difference(&sites.accounted).collect::<Vec<_>>();
    let phantom = sites.accounted.difference(&all).collect::<Vec<_>>();
    if !unaccounted.is_empty() || !phantom.is_empty() {
        sites.error = Some(format!(
            "{}; unaccounted critical tokens: {unaccounted:?}; unmatched AST tokens: {phantom:?}",
            sites
                .error
                .as_deref()
                .unwrap_or("unsupported callee/declaration form")
        ));
    }
    Ok(sites)
}

/// A conservative source universe: every Rust file under the binary directory.
/// Fail on filesystem indirection/read errors. The AST pass rejects include!
/// and #[path]; arbitrary macro expansion/synthesized identifiers are not proven.
pub fn source_inventory(root: &Path) -> Result<BTreeMap<String, String>, String> {
    fn walk(
        root: &Path,
        directory: &Path,
        files: &mut BTreeMap<String, String>,
    ) -> Result<(), String> {
        for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_symlink() {
                return Err(format!("source symlink is unsupported: {}", path.display()));
            }
            if !path
                .canonicalize()
                .map_err(|e| e.to_string())?
                .starts_with(root)
            {
                return Err(format!("source escaped inventory root: {}", path.display()));
            }
            if kind.is_dir() {
                walk(root, &path, files)?;
            } else if kind.is_file() {
                if path.extension().is_some_and(|ext| ext == "rs") {
                    let name = path
                        .strip_prefix(root)
                        .map_err(|e| e.to_string())?
                        .to_str()
                        .ok_or("non-UTF-8 source path")?
                        .to_string();
                    let source = std::fs::read_to_string(&path)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    if files.insert(name, source).is_some() {
                        return Err("duplicate source path".into());
                    }
                }
            } else {
                return Err(format!("unsupported source entry: {}", path.display()));
            }
        }
        Ok(())
    }
    if std::fs::symlink_metadata(root)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("source root is a symlink".into());
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let mut files = BTreeMap::new();
    walk(&root, &root, &mut files)?;
    if !files.contains_key("main.rs") {
        return Err("source inventory lacks main.rs".into());
    }
    Ok(files)
}

pub fn forwards_parameter(text: &str, function: &str, index: usize, callee: &str) -> bool {
    #[derive(Default)]
    struct SignatureAttributes(bool);
    impl<'ast> Visit<'ast> for SignatureAttributes {
        fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
            self.0 |= !path_is(attribute.path(), "doc");
        }
    }
    struct Calls<'a> {
        callee: &'a str,
        parameter: &'a str,
        arguments: Vec<TokenKey>,
        unsupported: bool,
    }
    impl<'ast> Visit<'ast> for Calls<'_> {
        fn visit_item(&mut self, _: &'ast syn::Item) {
            // Nested items have their own name-resolution scope; none occurs
            // in the two actual forwarding bodies. Do not interpret them.
            self.unsupported = true;
        }
        fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
            self.unsupported |= !path_is(attribute.path(), "doc");
        }
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if call_path(&call.func).as_deref() == Some(self.callee) {
                match call.args.first() {
                    Some(Expr::Path(path))
                        if call.attrs.is_empty()
                            && path.attrs.is_empty()
                            && path.qself.is_none()
                            && path
                                .path
                                .get_ident()
                                .is_some_and(|name| ident_is(name, self.parameter)) =>
                    {
                        self.arguments
                            .push(token_key(path.path.get_ident().unwrap()));
                    }
                    _ => self.unsupported = true,
                }
            }
            syn::visit::visit_expr_call(self, call);
        }
    }
    let Ok(file) = syn::parse_file(text) else {
        return false;
    };
    let functions = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Fn(item) if ident_is(&item.sig.ident, function) => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [function] = functions.as_slice() else {
        return false;
    };
    if function
        .attrs
        .iter()
        .any(|attr| !path_is(attr.path(), "doc"))
    {
        return false;
    }
    let mut attributes = SignatureAttributes::default();
    attributes.visit_signature(&function.sig);
    if attributes.0 {
        return false;
    }
    let Some(syn::FnArg::Typed(argument)) = function.sig.inputs.iter().nth(index) else {
        return false;
    };
    let syn::Pat::Ident(parameter) = argument.pat.as_ref() else {
        return false;
    };
    if parameter.mutability.is_some() || parameter.by_ref.is_some() || parameter.subpat.is_some() {
        return false;
    }
    let name = ident_name(&parameter.ident);
    // This narrow exemption is for the two actual `site` forwarding functions.
    // Require every occurrence, including opaque macro tokens and raw spelling,
    // to be exactly the declaration or the sole direct call argument. No local
    // alias/binder interpreter or macro/name-resolution claim is involved.
    if name != "site" || non_ascii_tokens(function.to_token_stream()) {
        return false;
    }
    fn occurrences(tokens: TokenStream, name: &str) -> BTreeSet<TokenKey> {
        let mut found = BTreeSet::new();
        for token in tokens {
            match token {
                TokenTree::Ident(ident) if ident_is(&ident, name) => {
                    found.insert(token_key(&ident));
                }
                TokenTree::Group(group) => found.extend(occurrences(group.stream(), name)),
                _ => {}
            }
        }
        found
    }
    let mut calls = Calls {
        callee,
        parameter: &name,
        arguments: Vec::new(),
        unsupported: false,
    };
    calls.visit_block(&function.block);
    let [forwarded] = calls.arguments.as_slice() else {
        return false;
    };
    let expected = BTreeSet::from([token_key(&parameter.ident), forwarded.clone()]);
    !calls.unsupported
        && expected.len() == 2
        && occurrences(function.to_token_stream(), &name) == expected
}

#[derive(Clone)]
struct Findings {
    labels: Vec<String>,
    error: Option<String>,
}

/// Test-local reuse of a pure parse/AST inspection. Keys own the entire source
/// bytes and relative filename (which is part of the forwarding proof). No
/// filesystem metadata, hash-only key, ambient context or cross-run state.
#[derive(Default)]
pub struct SourceAudit {
    inspected: BTreeMap<(String, String), Result<Findings, String>>,
    requests: usize,
}

impl SourceAudit {
    pub fn inspected_sources(&self) -> usize {
        self.inspected.len()
    }
    pub fn requests(&self) -> usize {
        self.requests
    }

    pub fn coverage<'a>(
        &mut self,
        sources: impl IntoIterator<Item = (&'a str, &'a str)>,
        covered: &[&str],
    ) -> Result<(), String> {
        let mut declared = Vec::new();
        let mut errors = Vec::new();
        for (file, source) in sources {
            self.requests += 1;
            let sites = self
                .inspected
                .entry((file.into(), source.into()))
                .or_insert_with(|| {
                    inspect(file, source).map(|sites| Findings {
                        labels: sites.labels,
                        error: sites.error,
                    })
                })
                .clone()?;
            if let Some(error) = sites.error {
                errors.push((file, error));
            }
            declared.extend(sites.labels.into_iter().map(|site| (file, site)));
        }
        if declared.is_empty() {
            return Err("extracted zero fault sites".into());
        }
        let uncovered = declared
            .iter()
            .filter(|(_, site)| !covered.contains(&site.as_str()))
            .collect::<Vec<_>>();
        let stale = covered
            .iter()
            .filter(|site| !declared.iter().any(|(_, found)| found == **site))
            .collect::<Vec<_>>();
        if !uncovered.is_empty() || !stale.is_empty() || !errors.is_empty() {
            return Err(format!(
                "uncovered actual sites: {uncovered:?}; stale test rows: {stale:?}; proof errors: {errors:?}"
            ));
        }
        Ok(())
    }
}
