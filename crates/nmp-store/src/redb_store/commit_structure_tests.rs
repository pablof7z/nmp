//! Source-shape falsifiers for the post-commit truth boundary (#909).
//!
//! This parses Rust syntax rather than grepping statement order. A restored
//! `commit; recover_outbox_lanes(..)` shape fails even though the fallible
//! work is hidden behind a function call.

use std::collections::BTreeSet;
use std::path::Path;

use syn::visit::{self, Visit};
use syn::{Block, Expr, ImplItemFn, ItemFn, Stmt};

const PRODUCTION_TRANSACTION_FILES: &[&str] = &[
    "commit.rs",
    "ingest_txn.rs",
    "event_ops.rs",
    "outbox_ops.rs",
    "write_ops.rs",
    "store.rs",
];

#[derive(Default)]
struct CommitCalls {
    raw: Vec<usize>,
    prepared: Vec<usize>,
}

impl<'ast> Visit<'ast> for CommitCalls {
    fn visit_expr(&mut self, expression: &'ast Expr) {
        if let Expr::MethodCall(call) = expression {
            if call.method == "commit" {
                self.raw.push(expression as *const Expr as usize);
            } else if call.method == "commit_prepared" {
                self.prepared.push(expression as *const Expr as usize);
            }
        } else if let Expr::Call(call) = expression {
            if let Expr::Path(path) = call.func.as_ref() {
                if path
                    .path
                    .segments
                    .last()
                    .is_some_and(|segment| segment.ident == "commit_prepared")
                {
                    self.prepared.push(expression as *const Expr as usize);
                }
            }
        }
        visit::visit_expr(self, expression);
    }
}

fn collect_tail_prepared(block: &Block, tails: &mut BTreeSet<usize>) {
    let Some(Stmt::Expr(expression, None)) = block.stmts.last() else {
        return;
    };
    collect_tail_expression(expression, tails);
}

fn collect_tail_expression(expression: &Expr, tails: &mut BTreeSet<usize>) {
    match expression {
        Expr::Call(call)
            if matches!(
                call.func.as_ref(),
                Expr::Path(path)
                    if path.path.segments.last().is_some_and(
                        |segment| segment.ident == "commit_prepared"
                    )
            ) =>
        {
            tails.insert(expression as *const Expr as usize);
        }
        Expr::MethodCall(call) if call.method == "commit_prepared" => {
            tails.insert(expression as *const Expr as usize);
        }
        Expr::Block(block) => collect_tail_prepared(&block.block, tails),
        Expr::If(branch) => {
            collect_tail_prepared(&branch.then_branch, tails);
            if let Some((_, otherwise)) = &branch.else_branch {
                collect_tail_expression(otherwise, tails);
            }
        }
        Expr::Match(branch) => {
            for arm in &branch.arms {
                collect_tail_expression(&arm.body, tails);
            }
        }
        Expr::Paren(inner) => collect_tail_expression(&inner.expr, tails),
        Expr::Group(inner) => collect_tail_expression(&inner.expr, tails),
        Expr::Return(ret) => {
            if let Some(value) = &ret.expr {
                collect_tail_expression(value, tails);
            }
        }
        _ => {}
    }
}

fn allowed_cfg_attributes(expression: &Expr) -> bool {
    let attributes = match expression {
        Expr::Block(block) => &block.attrs,
        Expr::MethodCall(call) => &call.attrs,
        _ => return false,
    };
    attributes.iter().any(|attribute| {
        if !attribute.path().is_ident("cfg") {
            return false;
        }
        let mut allowed = false;
        attribute
            .parse_nested_meta(|meta| {
                if meta.path.is_ident("test") {
                    allowed = true;
                } else if meta.path.is_ident("feature") {
                    let value = meta.value()?;
                    let feature: syn::LitStr = value.parse()?;
                    allowed |= feature.value() == "bench-instrumentation";
                }
                Ok(())
            })
            .expect("cfg syntax parses");
        allowed
    })
}

fn is_infallible_ok(statement: &Stmt) -> bool {
    matches!(
        statement,
        Stmt::Expr(
            Expr::Call(call),
            None
        ) if matches!(
            call.func.as_ref(),
            Expr::Path(path)
                if path.path.segments.last().is_some_and(|segment| segment.ident == "Ok")
        )
    )
}

fn non_tail_commit_has_only_cfg_instrumentation_after(block: &Block, commit: usize) -> bool {
    let Some(commit_index) = block.stmts.iter().position(|statement| {
        let mut calls = CommitCalls::default();
        calls.visit_stmt(statement);
        calls.prepared.contains(&commit)
    }) else {
        return false;
    };
    let after = &block.stmts[commit_index + 1..];
    let Some((last, middle)) = after.split_last() else {
        return false;
    };
    is_infallible_ok(last)
        && middle.iter().all(|statement| {
            matches!(
                statement,
                Stmt::Expr(expression, _) if allowed_cfg_attributes(expression)
            )
        })
}

fn inspect_function(file: &str, name: &str, block: &Block, failures: &mut Vec<String>) {
    let mut calls = CommitCalls::default();
    calls.visit_block(block);

    let raw_commit_allowed = matches!(
        (file, name),
        ("commit.rs", "commit_prepared")
            | ("ingest_txn.rs", "commit_prepared")
            | ("store.rs", "open_inner")
            | ("store.rs", "drop")
    );
    if !raw_commit_allowed && !calls.raw.is_empty() {
        failures.push(format!(
            "{file}::{name} contains a raw .commit(); EventStore mutations must use commit_prepared"
        ));
    }

    let mut tails = BTreeSet::new();
    collect_tail_prepared(block, &mut tails);
    for call in calls.prepared {
        if !tails.contains(&call)
            && !non_tail_commit_has_only_cfg_instrumentation_after(block, call)
        {
            failures.push(format!(
                "{file}::{name} calls commit_prepared outside a tail return position"
            ));
        }
    }
}

struct FunctionGate<'a> {
    file: &'a str,
    failures: Vec<String>,
}

impl<'ast> Visit<'ast> for FunctionGate<'_> {
    fn visit_item_fn(&mut self, function: &'ast ItemFn) {
        inspect_function(
            self.file,
            &function.sig.ident.to_string(),
            &function.block,
            &mut self.failures,
        );
        visit::visit_item_fn(self, function);
    }

    fn visit_impl_item_fn(&mut self, function: &'ast ImplItemFn) {
        inspect_function(
            self.file,
            &function.sig.ident.to_string(),
            &function.block,
            &mut self.failures,
        );
        visit::visit_impl_item_fn(self, function);
    }
}

fn parse_source(file: &str) -> syn::File {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/redb_store")
        .join(file);
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    syn::parse_file(&source).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn collect_variant_names(pattern: &syn::Pat, names: &mut BTreeSet<String>) {
    match pattern {
        syn::Pat::Or(alternatives) => {
            for pattern in &alternatives.cases {
                collect_variant_names(pattern, names);
            }
        }
        syn::Pat::Path(path) => {
            if let Some(segment) = path.path.segments.last() {
                names.insert(segment.ident.to_string());
            }
        }
        syn::Pat::Struct(structure) => {
            if let Some(segment) = structure.path.segments.last() {
                names.insert(segment.ident.to_string());
            }
        }
        syn::Pat::TupleStruct(tuple) => {
            if let Some(segment) = tuple.path.segments.last() {
                names.insert(segment.ident.to_string());
            }
        }
        syn::Pat::Wild(_) => {}
        _ => panic!("unexpected classify pattern"),
    }
}

#[test]
fn event_store_commits_have_no_fallible_post_commit_reachability() {
    let mut failures = Vec::new();
    for file in PRODUCTION_TRANSACTION_FILES {
        let syntax = parse_source(file);
        let mut gate = FunctionGate {
            file,
            failures: Vec::new(),
        };
        gate.visit_file(&syntax);
        failures.extend(gate.failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn future_redb_errors_use_the_conservative_fallback() {
    let syntax = parse_source("schema.rs");
    let classify = syntax
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == "classify" => Some(function),
            _ => None,
        })
        .expect("schema owns one classify function");
    let Some(Stmt::Expr(Expr::Match(classification), None)) = classify.block.stmts.last() else {
        panic!("classify must remain one exhaustive match");
    };
    let wildcard = classification
        .arms
        .iter()
        .filter(|arm| matches!(arm.pat, syn::Pat::Wild(_)))
        .collect::<Vec<_>>();
    assert_eq!(
        wildcard.len(),
        1,
        "classify must have one future-variant arm"
    );
    assert!(
        matches!(
            wildcard[0].body.as_ref(),
            Expr::Call(call)
                if matches!(
                    call.func.as_ref(),
                    Expr::Path(path)
                        if path.path.segments.last().is_some_and(
                            |segment| segment.ident == "unknown_backend_fault"
                        )
                )
        ),
        "the non-exhaustive redb fallback must map through unknown_backend_fault"
    );

    let mut explicit = BTreeSet::new();
    for arm in &classification.arms {
        collect_variant_names(&arm.pat, &mut explicit);
    }
    let expected = BTreeSet::from([
        "Corrupted",
        "DatabaseAlreadyOpen",
        "DatabaseClosed",
        "EphemeralSavepointExists",
        "ImmediateDurabilityRequired",
        "InvalidSavepoint",
        "Io",
        "LockPoisoned",
        "PersistentSavepointExists",
        "PersistentSavepointModified",
        "PreviousIo",
        "ReadTransactionStillInUse",
        "RepairAborted",
        "TableAlreadyOpen",
        "TableDoesNotExist",
        "TableExists",
        "TableIsMultimap",
        "TableIsNotMultimap",
        "TableTypeMismatch",
        "TransactionInProgress",
        "TypeDefinitionChanged",
        "UpgradeRequired",
        "ValueTooLarge",
    ])
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(
        explicit, expected,
        "redb 4.1's complete current error table must remain explicit"
    );
}
