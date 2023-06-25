use crate::database::{Database, DbType, FileIndex, Namespace, PointLink};

use crate::debug;
use crate::file::File;
use crate::file::PythonFile;
use crate::imports::{python_import, ImportResult};
use crate::inference_state::InferenceState;

use crate::inferred::Inferred;
use crate::matching::{LookupResult, Type};
use crate::node_ref::NodeRef;

#[derive(Copy, Clone)]
pub struct Module<'a> {
    pub file: &'a PythonFile,
}

impl<'a> Module<'a> {
    pub fn new(file: &'a PythonFile) -> Self {
        Self { file }
    }

    pub fn from_file_index(db: &'a Database, file_index: FileIndex) -> Self {
        Self::new(db.loaded_python_file(file_index))
    }

    pub fn sub_module(&self, db: &'a Database, name: &str) -> Option<ImportResult> {
        self.file.package_dir.as_ref().and_then(|dir| {
            let p = db.vfs.dir_path(self.file.file_path(db)).unwrap();
            python_import(db, self.file.file_index(), p, dir, name)
        })
    }

    pub fn name(&self, db: &'a Database) -> &'a str {
        // TODO this is not correct...
        let (dir, mut name) = db.vfs.dir_and_name(self.file.file_path(db));
        if let Some(n) = name.strip_suffix(".py") {
            name = n
        } else {
            name = name.trim_end_matches(".pyi");
        }
        if name == "__init__" {
            db.vfs.dir_and_name(dir.unwrap()).1
        } else {
            name
        }
    }

    pub fn qualified_name(&self, db: &Database) -> String {
        self.name(db).to_owned()
    }

    pub fn lookup(
        &self,
        i_s: &InferenceState,
        node_ref: Option<NodeRef>,
        name: &str,
    ) -> LookupResult {
        self.file
            .symbol_table
            .lookup_symbol(name)
            .map(|i| {
                LookupResult::GotoName(
                    PointLink::new(self.file.file_index(), i),
                    self.file.inference(i_s).infer_name_by_index(i),
                )
            })
            .or_else(|| {
                self.sub_module(i_s.db, name).map(|result| match result {
                    ImportResult::File(file_index) => LookupResult::FileReference(file_index),
                    ImportResult::Namespace { .. } => todo!(),
                })
            })
            .unwrap_or_else(|| {
                Type::owned(i_s.db.python_state.module_db_type()).lookup_without_error(i_s, name)
            })
    }
}

pub fn lookup_in_namespace(
    db: &Database,
    from_file: FileIndex,
    namespace: &Namespace,
    name: &str,
) -> LookupResult {
    match python_import(db, from_file, &namespace.path, &namespace.content, name) {
        Some(ImportResult::File(file_index)) => LookupResult::FileReference(file_index),
        Some(ImportResult::Namespace(namespace)) => {
            LookupResult::UnknownName(Inferred::from_type(DbType::Namespace(namespace)))
        }
        None => {
            debug!("TODO namespace basic lookups");
            LookupResult::None
        }
    }
}
