//! Defines a unit of change that can applied to a state of IDE to get the next
//! state. Changes are transactional.

use std::{fmt, sync::Arc, time};

use ra_db::{
    salsa::{Database, Durability, SweepStrategy},
    CrateGraph, FileId, RelativePathBuf, SourceDatabase, SourceDatabaseExt, SourceRoot,
    SourceRootId,
};
use ra_prof::{memory_usage, profile, Bytes};
use rustc_hash::FxHashSet;

use crate::{symbol_index::SymbolsDatabase, RootDatabase};

#[derive(Default)]
pub struct AnalysisChange {
    roots: Option<Vec<SourceRoot>>,
    files_changed: Vec<(FileId, Option<Arc<String>>)>,
    crate_graph: Option<CrateGraph>,
}

impl fmt::Debug for AnalysisChange {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let mut d = fmt.debug_struct("AnalysisChange");
        if let Some(roots) = &self.roots {
            d.field("roots", roots);
        }
        if !self.files_changed.is_empty() {
            d.field("files_changed", &self.files_changed.len());
        }
        if self.crate_graph.is_some() {
            d.field("crate_graph", &self.crate_graph);
        }
        d.finish()
    }
}

impl AnalysisChange {
    pub fn new() -> AnalysisChange {
        AnalysisChange::default()
    }

    pub fn set_roots(&mut self, roots: Vec<SourceRoot>) {
        self.roots = Some(roots);
    }

    pub fn change_file(&mut self, file_id: FileId, new_text: Option<Arc<String>>) {
        self.files_changed.push((file_id, new_text))
    }

    pub fn set_crate_graph(&mut self, graph: CrateGraph) {
        self.crate_graph = Some(graph);
    }
}

#[derive(Debug)]
struct AddFile {
    file_id: FileId,
    path: RelativePathBuf,
    text: Arc<String>,
}

#[derive(Debug)]
struct RemoveFile {
    file_id: FileId,
    path: RelativePathBuf,
}

#[derive(Default)]
struct RootChange {
    added: Vec<AddFile>,
    removed: Vec<RemoveFile>,
}

impl fmt::Debug for RootChange {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("AnalysisChange")
            .field("added", &self.added.len())
            .field("removed", &self.removed.len())
            .finish()
    }
}

const GC_COOLDOWN: time::Duration = time::Duration::from_millis(100);

impl RootDatabase {
    pub fn request_cancellation(&mut self) {
        let _p = profile("RootDatabase::request_cancellation");
        self.salsa_runtime_mut().synthetic_write(Durability::LOW);
    }

    pub fn apply_change(&mut self, change: AnalysisChange) {
        let _p = profile("RootDatabase::apply_change");
        self.request_cancellation();
        log::info!("apply_change {:?}", change);
        if let Some(roots) = change.roots {
            let mut local_roots = FxHashSet::default();
            let mut library_roots = FxHashSet::default();
            for (idx, root) in roots.into_iter().enumerate() {
                let root_id = SourceRootId(idx as u32);
                let durability = durability(&root);
                if root.is_library {
                    library_roots.insert(root_id);
                } else {
                    local_roots.insert(root_id);
                }
                for file_id in root.iter() {
                    self.set_file_source_root_with_durability(file_id, root_id, durability);
                }
                self.set_source_root_with_durability(root_id, Arc::new(root), durability);
            }
            self.set_local_roots_with_durability(Arc::new(local_roots), Durability::HIGH);
            self.set_library_roots_with_durability(Arc::new(library_roots), Durability::HIGH);
        }

        for (file_id, text) in change.files_changed {
            let source_root_id = self.file_source_root(file_id);
            let source_root = self.source_root(source_root_id);
            let durability = durability(&source_root);
            // XXX: can't actually remove the file, just reset the text
            let text = text.unwrap_or_default();
            self.set_file_text_with_durability(file_id, text, durability)
        }
        if let Some(crate_graph) = change.crate_graph {
            self.set_crate_graph_with_durability(Arc::new(crate_graph), Durability::HIGH)
        }
    }

    pub fn maybe_collect_garbage(&mut self) {
        if cfg!(feature = "wasm") {
            return;
        }

        if self.last_gc_check.elapsed() > GC_COOLDOWN {
            self.last_gc_check = crate::wasm_shims::Instant::now();
        }
    }

    pub fn collect_garbage(&mut self) {
        if cfg!(feature = "wasm") {
            return;
        }

        let _p = profile("RootDatabase::collect_garbage");
        self.last_gc = crate::wasm_shims::Instant::now();

        let sweep = SweepStrategy::default().discard_values().sweep_all_revisions();

        self.query(ra_db::ParseQuery).sweep(sweep);
        self.query(hir::db::ParseMacroQuery).sweep(sweep);

        // Macros do take significant space, but less then the syntax trees
        // self.query(hir::db::MacroDefQuery).sweep(sweep);
        // self.query(hir::db::MacroArgQuery).sweep(sweep);
        // self.query(hir::db::MacroExpandQuery).sweep(sweep);

        self.query(hir::db::AstIdMapQuery).sweep(sweep);

        self.query(hir::db::BodyWithSourceMapQuery).sweep(sweep);

        self.query(hir::db::ExprScopesQuery).sweep(sweep);
        self.query(hir::db::InferQueryQuery).sweep(sweep);
        self.query(hir::db::BodyQuery).sweep(sweep);
    }

    pub fn per_query_memory_usage(&mut self) -> Vec<(String, Bytes)> {
        let mut acc: Vec<(String, Bytes)> = vec![];
        let sweep = SweepStrategy::default().discard_values().sweep_all_revisions();
        macro_rules! sweep_each_query {
            ($($q:path)*) => {$(
                let before = memory_usage().allocated;
                self.query($q).sweep(sweep);
                let after = memory_usage().allocated;
                let q: $q = Default::default();
                let name = format!("{:?}", q);
                acc.push((name, before - after));

                let before = memory_usage().allocated;
                self.query($q).sweep(sweep.discard_everything());
                let after = memory_usage().allocated;
                let q: $q = Default::default();
                let name = format!("{:?} (deps)", q);
                acc.push((name, before - after));
            )*}
        }
        sweep_each_query![
            // SourceDatabase
            ra_db::ParseQuery
            ra_db::SourceRootCratesQuery

            // AstDatabase
            hir::db::AstIdMapQuery
            hir::db::InternMacroQuery
            hir::db::MacroArgQuery
            hir::db::MacroDefQuery
            hir::db::ParseMacroQuery
            hir::db::MacroExpandQuery
            hir::db::InternEagerExpansionQuery

            // DefDatabase
            hir::db::CrateDefMapQueryQuery
            hir::db::StructDataQuery
            hir::db::UnionDataQuery
            hir::db::EnumDataQuery
            hir::db::ImplDataQuery
            hir::db::TraitDataQuery
            hir::db::TypeAliasDataQuery
            hir::db::FunctionDataQuery
            hir::db::ConstDataQuery
            hir::db::StaticDataQuery
            hir::db::BodyWithSourceMapQuery
            hir::db::BodyQuery
            hir::db::ExprScopesQuery
            hir::db::GenericParamsQuery
            hir::db::AttrsQuery
            hir::db::ModuleLangItemsQuery
            hir::db::CrateLangItemsQuery
            hir::db::LangItemQuery
            hir::db::DocumentationQuery
            hir::db::ImportMapQuery

            // InternDatabase
            hir::db::InternFunctionQuery
            hir::db::InternStructQuery
            hir::db::InternUnionQuery
            hir::db::InternEnumQuery
            hir::db::InternConstQuery
            hir::db::InternStaticQuery
            hir::db::InternTraitQuery
            hir::db::InternTypeAliasQuery
            hir::db::InternImplQuery

            // HirDatabase
            hir::db::InferQueryQuery
            hir::db::TyQuery
            hir::db::ValueTyQuery
            hir::db::ImplSelfTyQuery
            hir::db::ImplTraitQuery
            hir::db::FieldTypesQuery
            hir::db::CallableItemSignatureQuery
            hir::db::GenericPredicatesForParamQuery
            hir::db::GenericPredicatesQuery
            hir::db::GenericDefaultsQuery
            hir::db::ImplsInCrateQuery
            hir::db::ImplsFromDepsQuery
            hir::db::InternTypeCtorQuery
            hir::db::InternTypeParamIdQuery
            hir::db::InternChalkImplQuery
            hir::db::InternAssocTyValueQuery
            hir::db::AssociatedTyDataQuery
            hir::db::TraitDatumQuery
            hir::db::StructDatumQuery
            hir::db::ImplDatumQuery
            hir::db::AssociatedTyValueQuery
            hir::db::TraitSolveQuery
            hir::db::ReturnTypeImplTraitsQuery

            // SymbolsDatabase
            crate::symbol_index::FileSymbolsQuery

            // LineIndexDatabase
            crate::LineIndexQuery
        ];
        acc.sort_by_key(|it| std::cmp::Reverse(it.1));
        acc
    }
}

fn durability(source_root: &SourceRoot) -> Durability {
    if source_root.is_library {
        Durability::HIGH
    } else {
        Durability::LOW
    }
}
