//! Source-shape falsifiers for the post-commit truth boundary (#909).
//!
//! This parses Rust syntax rather than grepping statement order. A restored
//! `commit; recover_publish_queue_lanes(..)` shape fails even though the fallible
//! work is hidden behind a function call.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use syn::visit::{self, Visit};
use syn::{Block, Expr, ImplItemFn, ItemFn, Stmt};

const PRODUCTION_TRANSACTION_FILES: &[&str] = &[
    "commit.rs",
    "ingest_txn.rs",
    "event_ops.rs",
    "publish_queue_ops.rs",
    "write_ops.rs",
    "store.rs",
];

const RAW_COMMIT_EXECUTORS: &[(&str, &str)] = &[
    ("commit.rs", "commit_prepared"),
    ("ingest_txn.rs", "commit_prepared"),
];

const RAW_COMMIT_EXEMPTIONS: &[(&str, &str)] = &[("store.rs", "open_inner"), ("store.rs", "drop")];

#[derive(Default)]
struct CommitCalls {
    raw: Vec<usize>,
    prepared: Vec<usize>,
    begins_transaction: bool,
}

impl<'ast> Visit<'ast> for CommitCalls {
    fn visit_expr(&mut self, expression: &'ast Expr) {
        if let Expr::MethodCall(call) = expression {
            if call.method == "begin_write" {
                self.begins_transaction = true;
            } else if call.method == "commit" {
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
        Expr::Call(call) => &call.attrs,
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

#[derive(Clone, Copy)]
enum CommitCallKind {
    Raw,
    Prepared,
}

fn commit_has_only_cfg_instrumentation_and_ok_after(
    block: &Block,
    commit: usize,
    kind: CommitCallKind,
) -> bool {
    let Some(commit_index) = block.stmts.iter().position(|statement| {
        let mut calls = CommitCalls::default();
        calls.visit_stmt(statement);
        match kind {
            CommitCallKind::Raw => calls.raw.contains(&commit),
            CommitCallKind::Prepared => calls.prepared.contains(&commit),
        }
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

    let identity = (file, name);
    if RAW_COMMIT_EXECUTORS.contains(&identity) {
        if calls.raw.len() != 1 {
            failures.push(format!(
                "{file}::{name} must contain exactly one raw .commit()"
            ));
        }
        for call in calls.raw {
            if !commit_has_only_cfg_instrumentation_and_ok_after(block, call, CommitCallKind::Raw) {
                failures.push(format!(
                    "{file}::{name} has fallible or non-instrumentation work after its raw .commit()"
                ));
            }
        }
    } else if !RAW_COMMIT_EXEMPTIONS.contains(&identity) && !calls.raw.is_empty() {
        failures.push(format!(
            "{file}::{name} contains a raw .commit(); EventStore mutations must use commit_prepared"
        ));
    }

    let mut tails = BTreeSet::new();
    collect_tail_prepared(block, &mut tails);
    for call in calls.prepared {
        if !tails.contains(&call)
            && !commit_has_only_cfg_instrumentation_and_ok_after(
                block,
                call,
                CommitCallKind::Prepared,
            )
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

fn source_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/redb_store")
}

fn parse_source_path(path: &Path) -> syn::File {
    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    syn::parse_file(&source).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn parse_source(file: &str) -> syn::File {
    parse_source_path(&source_dir().join(file))
}

fn is_production_source(file: &str) -> bool {
    file.ends_with(".rs")
        && file != "tests.rs"
        && !file.ends_with("_tests.rs")
        && !file.ends_with("_bench.rs")
}

fn production_transaction_files() -> BTreeSet<String> {
    std::fs::read_dir(source_dir())
        .expect("read redb_store source directory")
        .map(|entry| entry.expect("read redb_store source entry").path())
        .filter(|path| path.is_file())
        .filter_map(|path| {
            let file = path.file_name()?.to_str()?.to_owned();
            is_production_source(&file).then_some((file, path))
        })
        .filter_map(|(file, path)| {
            let syntax = parse_source_path(&path);
            let mut calls = CommitCalls::default();
            calls.visit_file(&syntax);
            (calls.begins_transaction || !calls.raw.is_empty() || !calls.prepared.is_empty())
                .then_some(file)
        })
        .collect()
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
fn production_transaction_file_census_fails_closed() {
    let expected = PRODUCTION_TRANSACTION_FILES
        .iter()
        .map(|file| (*file).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        production_transaction_files(),
        expected,
        "every production module that begins or commits a redb transaction must be in the structural gate"
    );
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
fn executor_body_rejects_fallible_work_after_raw_commit() {
    let mutated = syn::parse_str::<ItemFn>(
        r#"
        fn commit_prepared<T>(
            write_txn: redb::WriteTransaction,
            prepared: T,
        ) -> Result<T, PersistenceError> {
            write_txn.commit().map_err(persist_err)?;
            std::fs::metadata(".").map_err(|error| {
                PersistenceError::invariant(error.to_string())
            })?;
            Ok(prepared)
        }
        "#,
    )
    .expect("adversarial executor mutation parses");

    for file in ["commit.rs", "ingest_txn.rs"] {
        let mut failures = Vec::new();
        inspect_function(file, "commit_prepared", &mutated.block, &mut failures);
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("work after its raw .commit()")),
            "{file}'s raw commit executor must reject restored fallible work: {failures:?}"
        );
    }
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
