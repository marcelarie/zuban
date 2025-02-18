use std::{borrow::Borrow, rc::Rc};

use utils::match_case;
use vfs::{Directory, DirectoryEntry, FileIndex, WorkspaceKind};

use crate::{
    database::Database,
    file::{File, PythonFile},
    inferred::Inferred,
    type_::{Namespace, Type},
    type_helpers::Module,
};

pub const STUBS_SUFFIX: &str = "-stubs";
const INIT_PY: &str = "__init__.py";
const INIT_PYI: &str = "__init__.pyi";

#[derive(Debug)]
pub enum ImportResult {
    File(FileIndex),
    Namespace(Rc<Namespace>), // A Python Namespace package, i.e. a directory
    PyTypedMissing,           // Files exist, but the py.typed marker is missing.
}

impl ImportResult {
    pub fn import(
        &self,
        db: &Database,
        original_file: &PythonFile,
        name: &str,
    ) -> Option<ImportResult> {
        match self {
            Self::File(file_index) => {
                let module = Module::from_file_index(db, *file_index);
                module.sub_module(db, name)
            }
            Self::Namespace(ns) => {
                python_import(db, original_file, ns.directories.iter().cloned(), name)
            }
            Self::PyTypedMissing => unreachable!(),
        }
    }

    pub(crate) fn import_non_stub_for_stub_package(
        db: &Database,
        original_file: &PythonFile,
        parent_dir: Option<Rc<Directory>>,
        name: &str,
    ) -> Option<Self> {
        if let Some(parent_dir) = parent_dir {
            Self::import_non_stub_for_stub_package(
                db,
                original_file,
                parent_dir.parent.maybe_dir().ok(),
                &parent_dir.name,
            )?
            .import(db, original_file, name)
        } else {
            let name = name.strip_suffix(STUBS_SUFFIX)?;
            global_import_without_stubs_first(db, original_file, name)
        }
    }

    pub fn as_inferred(&self) -> Inferred {
        match self {
            ImportResult::File(file_index) => Inferred::new_file_reference(*file_index),
            ImportResult::Namespace(namespace) => {
                Inferred::from_type(Type::Namespace(namespace.clone()))
            }
            Self::PyTypedMissing => Inferred::new_any_from_error(),
        }
    }

    pub fn qualified_name(&self, db: &Database) -> String {
        match self {
            Self::File(file_index) => db.loaded_python_file(*file_index).qualified_name(db),
            Self::Namespace(ns) => ns.qualified_name(),
            Self::PyTypedMissing => unreachable!(),
        }
    }

    pub fn debug_info<'x>(&'x self, db: &'x Database) -> String {
        match self {
            Self::File(f) => format!("{} ({f})", db.loaded_python_file(*f).file_path(db)),
            Self::Namespace(namespace) => {
                format!("namespace {}", namespace.debug_path(db))
            }
            Self::PyTypedMissing => "<py.typed missing>".into(),
        }
    }
}

pub fn global_import<'a>(
    db: &'a Database,
    from_file: &PythonFile,
    name: &'a str,
) -> Option<ImportResult> {
    // First try <package>-stubs
    global_import_without_stubs_first(db, from_file, &format!("{name}{STUBS_SUFFIX}")).or_else(
        || {
            python_import_with_needs_exact_case(
                db,
                from_file,
                db.vfs
                    .workspaces
                    .iter()
                    .map(|w| (&w.directory, matches!(w.kind, WorkspaceKind::SitePackages))),
                name,
                false,
            )
        },
    )
}

pub fn global_import_without_stubs_first<'a>(
    db: &'a Database,
    from_file: &PythonFile,
    name: &'a str,
) -> Option<ImportResult> {
    python_import(
        db,
        from_file,
        db.vfs.workspaces.iter().map(|d| &d.directory),
        name,
    )
}

pub fn namespace_import(
    db: &Database,
    from_file: &PythonFile,
    namespace: &Namespace,
    name: &str,
) -> Option<ImportResult> {
    let result =
        python_import(db, from_file, namespace.directories.iter().cloned(), name).or_else(|| {
            // If the namespace does not have a specific import, we check if we are in a
            // <foo>-stubs package and import the non-stubs version of that package.
            namespace
                .directories
                .iter()
                .filter_map(|dir| {
                    ImportResult::import_non_stub_for_stub_package(
                        db,
                        from_file,
                        Some(dir.clone()),
                        name,
                    )
                })
                .next()
        });
    // Since we are in a namespace, we need to verify the case where a namespace within
    // site-packages has a py.typed in one of the subdirectories.
    if let Some(ImportResult::File(file_index)) = result {
        let file = db.loaded_python_file(file_index);
        let mut parent = file.file_entry(db).parent.clone();
        loop {
            match parent.maybe_dir() {
                Ok(dir) => {
                    if dir.search("py.typed").is_some() || dir.name.ends_with(STUBS_SUFFIX) {
                        return result;
                    }
                    parent = dir.parent.clone();
                }
                Err(workspace_root) => {
                    for workspace in db.vfs.workspaces.iter() {
                        if *workspace.root_path() == **workspace_root {
                            if workspace.kind == WorkspaceKind::SitePackages {
                                return Some(ImportResult::PyTypedMissing);
                            } else {
                                return result;
                            }
                        }
                    }
                    unreachable!()
                }
            }
        }
    }
    result
}

fn python_import(
    db: &Database,
    from_file: &PythonFile,
    dirs: impl Iterator<Item = impl Borrow<Directory>>,
    name: &str,
) -> Option<ImportResult> {
    python_import_with_needs_exact_case(db, from_file, dirs.map(|d| (d, false)), name, false)
}

pub fn python_import_with_needs_exact_case(
    db: &Database,
    from_file: &PythonFile,
    // Directory / Needs py.typed pairing
    dirs: impl Iterator<Item = (impl Borrow<Directory>, bool)>,
    name: &str,
    needs_exact_case: bool,
) -> Option<ImportResult> {
    let mut python_file_index = None;
    let mut stub_file_index = None;
    let mut namespace_directories = vec![];

    let name_py = format!("{name}.py");
    let name_pyi = format!("{name}.pyi");

    for (dir, needs_py_typed) in dirs {
        let mut had_namespace_dir = false;
        let dir = dir.borrow();
        for entry in &dir.iter() {
            match entry {
                DirectoryEntry::Directory(dir2) => {
                    if match_c(db, dir2.name.as_ref(), name, needs_exact_case) {
                        let result = load_init_file(db, dir2, from_file.file_index);
                        if let Some(file_index) = result {
                            if needs_py_typed
                                && !from_file.flags(db).follow_untyped_imports
                                && dir2.search("py.typed").is_none()
                            {
                                return Some(ImportResult::PyTypedMissing);
                            }
                            return Some(ImportResult::File(file_index));
                        }
                        had_namespace_dir = true;
                        namespace_directories.push(dir2.clone());
                    }
                }
                DirectoryEntry::File(file) => {
                    // TODO these format!() always allocate a lot and don't seem to be necessary
                    let is_py_file = match_c(db, &file.name, &name_py, needs_exact_case);
                    if is_py_file || match_c(db, &file.name, &name_pyi, needs_exact_case) {
                        if needs_py_typed && !from_file.flags(db).follow_untyped_imports {
                            return Some(ImportResult::PyTypedMissing);
                        }
                        let file_index = db.load_file_from_workspace(file, false);
                        if is_py_file {
                            python_file_index = file_index.map(|f| (file.clone(), f));
                        } else {
                            stub_file_index = file_index.map(|f| (file.clone(), f));
                        }
                    }
                }
                DirectoryEntry::MissingEntry { .. } => (),
            }
        }
        if let Some((file_entry, file_index)) = stub_file_index.take().or(python_file_index.take())
        {
            file_entry.add_invalidation(from_file.file_index);
            return Some(ImportResult::File(file_index));
        }
        dir.add_missing_entry(&name_py, from_file.file_index);
        dir.add_missing_entry(&name_pyi, from_file.file_index);
        // The folder should not exist for folder/__init__.py or a namespace.
        if !had_namespace_dir {
            dir.add_missing_entry(name, from_file.file_index);
        }
    }
    if !namespace_directories.is_empty() {
        return Some(ImportResult::Namespace(Rc::new(Namespace {
            directories: namespace_directories.into(),
        })));
    }
    None
}

#[inline]
fn match_c(db: &Database, x: &str, y: &str, needs_exact_case: bool) -> bool {
    if needs_exact_case {
        x == y
    } else {
        match_case(db.project.flags.case_sensitive, x, y)
    }
}

fn load_init_file(db: &Database, content: &Directory, from_file: FileIndex) -> Option<FileIndex> {
    for child in &content.iter() {
        if let DirectoryEntry::File(entry) = child {
            if match_c(db, &entry.name, INIT_PY, false) || match_c(db, &entry.name, INIT_PYI, false)
            {
                let found_file_index = db.load_file_from_workspace(entry, false);
                entry.add_invalidation(from_file);
                return found_file_index;
            }
        }
    }
    content.add_missing_entry(INIT_PY, from_file);
    content.add_missing_entry(INIT_PYI, from_file);
    None
}

pub fn find_ancestor(db: &Database, file: &PythonFile, level: usize) -> Option<ImportResult> {
    debug_assert!(level > 0);
    let mut parent = file.file_entry(db).parent.maybe_dir().ok()?;
    for _ in 1..level {
        parent = parent.parent.maybe_dir().ok()?;
    }
    Some(match load_init_file(db, &parent, file.file_index) {
        Some(index) => ImportResult::File(index),
        None => ImportResult::Namespace(Rc::new(Namespace {
            directories: [parent].into(),
        })),
    })
}
