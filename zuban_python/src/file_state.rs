use std::any::Any;
use std::fmt;
use std::fs;
use std::pin::Pin;
use std::rc::Rc;

use crate::database::{Database, FileIndex};
use crate::diagnostics::{Diagnostic, DiagnosticConfig};
use crate::file::PythonFile;
use crate::inferred::Inferred;
use crate::name::{Name, Names, TreePosition};
use crate::utils::Invalidations;
use crate::workspaces::DirContent;
use parsa_python_ast::{CodeIndex, Keyword, NodeIndex};

type InvalidatedDependencies = Vec<FileIndex>;
type LoadFileFunction<F> = &'static dyn Fn(String) -> F;

pub trait Vfs {
    fn read_file(&self, path: &str) -> Option<String>;

    fn separator(&self) -> char {
        '/'
    }

    fn split_off_folder<'a>(&self, path: &'a str) -> (&'a str, Option<&'a str>) {
        if let Some(pos) = path.find(self.separator()) {
            (&path[..pos], Some(&path[pos + 1..]))
        } else {
            (path, None)
        }
    }

    fn dir_and_name<'a>(&self, path: &'a str) -> (Option<&'a str>, &'a str) {
        if let Some(pos) = path.rfind(self.separator()) {
            (Some(&path[..pos]), &path[pos + 1..])
        } else {
            (None, path)
        }
    }

    fn dir_path<'a>(&self, path: &'a str) -> Option<&'a str> {
        path.rfind(self.separator()).map(|index| &path[..index])
    }
}

#[derive(Default)]
pub struct FileSystemReader {}

impl Vfs for FileSystemReader {
    fn read_file(&self, path: &str) -> Option<String> {
        // TODO can error
        Some(fs::read_to_string(path).unwrap())
    }
}

#[derive(Debug)]
pub enum Leaf<'db> {
    Name(Box<dyn Name<'db> + 'db>),
    String,
    Number,
    Keyword(Keyword<'db>),
    None,
}

pub trait FileStateLoader {
    fn responsible_for_file_endings(&self) -> Vec<&str>;

    fn might_be_relevant(&self, name: &str) -> bool;
    fn should_be_ignored(&self, name: &str) -> bool;

    fn load_parsed(
        &self,
        dir: Rc<DirContent>,
        path: String,
        code: String,
    ) -> Pin<Box<dyn FileState>>;
}

#[derive(Default)]
pub struct PythonFileLoader {}

impl FileStateLoader for PythonFileLoader {
    fn responsible_for_file_endings(&self) -> Vec<&str> {
        vec!["py", "pyi"]
    }

    fn might_be_relevant(&self, name: &str) -> bool {
        if name.ends_with(".py") {
            return true;
        }
        !name.contains('.') && !name.contains('-')
    }

    fn should_be_ignored(&self, name: &str) -> bool {
        name == "__pycache__" && !name.ends_with(".pyc")
    }

    fn load_parsed(
        &self,
        dir: Rc<DirContent>,
        path: String,
        code: String,
    ) -> Pin<Box<dyn FileState>> {
        // TODO this check is really stupid.
        let package_dir =
            (path.ends_with("/__init__.py") || path.ends_with("/__init__.pyi")).then(|| dir);
        Box::pin(LanguageFileState::new_parsed(
            path,
            PythonFile::new(package_dir, code),
        ))
    }
}

pub trait AsAny {
    fn as_any(&self) -> &dyn Any
    where
        Self: 'static;
}

impl<T> AsAny for T {
    fn as_any(&self) -> &dyn Any
    where
        Self: 'static,
    {
        self
    }
}

pub trait File: std::fmt::Debug + AsAny {
    // Called each time a file is loaded
    fn ensure_initialized(&self);
    fn implementation<'db>(&self, names: Names<'db>) -> Names<'db> {
        vec![]
    }
    fn leaf<'db>(&'db self, db: &'db Database, position: CodeIndex) -> Leaf<'db>;
    fn infer_operator_leaf<'db>(&'db self, db: &'db Database, keyword: Keyword<'db>) -> Inferred;
    fn file_index(&self) -> FileIndex;
    fn set_file_index(&self, index: FileIndex);

    fn node_start_position(&self, n: NodeIndex) -> TreePosition;
    fn node_end_position(&self, n: NodeIndex) -> TreePosition;
    fn line_column_to_byte(&self, line: usize, column: usize) -> CodeIndex;
    fn byte_to_line_column(&self, byte: CodeIndex) -> (usize, usize);

    fn file_path<'db>(&self, db: &'db Database) -> &'db str {
        db.file_path(self.file_index())
    }

    fn diagnostics<'db>(
        &'db self,
        db: &'db Database,
        config: &DiagnosticConfig,
    ) -> Box<[Diagnostic<'db>]>;
    fn invalidate_references_to(&mut self, file_index: FileIndex);
}

pub trait FileState: fmt::Debug + Unpin {
    fn path(&self) -> &str;
    fn file(&self, reader: &dyn Vfs) -> Option<&(dyn File + 'static)>;
    fn maybe_loaded_file_mut(&mut self) -> Option<&mut dyn File>;
    fn set_file_index(&self, index: FileIndex);
    fn unload_and_return_invalidations(&mut self) -> Invalidations;
    fn add_invalidates(&self, file_index: FileIndex);
    fn take_invalidations(&mut self) -> Invalidations;
}

impl<F: File + Unpin> FileState for LanguageFileState<F> {
    fn path(&self) -> &str {
        &self.path
    }

    fn file(&self, file_system_reader: &dyn Vfs) -> Option<&(dyn File + 'static)> {
        match &self.state {
            InternalFileExistence::Unloaded => None,
            InternalFileExistence::Parsed(f) => Some(f),
        }
    }

    fn maybe_loaded_file_mut(&mut self) -> Option<&mut dyn File> {
        match &mut self.state {
            InternalFileExistence::Parsed(f) => Some(f),
            _ => None,
        }
    }

    fn set_file_index(&self, index: FileIndex) {
        match &self.state {
            InternalFileExistence::Unloaded => {}
            InternalFileExistence::Parsed(f) => f.set_file_index(index),
        }
    }

    fn unload_and_return_invalidations(&mut self) -> Invalidations {
        let invalidates = std::mem::take(&mut self.invalidates);
        self.state = InternalFileExistence::Unloaded;
        invalidates
    }

    fn take_invalidations(&mut self) -> Invalidations {
        std::mem::take(&mut self.invalidates)
    }

    fn add_invalidates(&self, file_index: FileIndex) {
        self.invalidates.add(file_index)
    }
}

pub struct LanguageFileState<F: 'static> {
    path: String,
    state: InternalFileExistence<F>,
    invalidates: Invalidations,
}

impl<F> fmt::Debug for LanguageFileState<F> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("LanguageFileState")
            .field("path", &self.path)
            .field("state", &self.state)
            .field("invalidates", &self.invalidates)
            .finish()
    }
}

impl<F: File> LanguageFileState<F> {
    pub fn new_parsed(path: String, file: F) -> Self {
        Self {
            path,
            state: InternalFileExistence::Parsed(file),
            invalidates: Default::default(),
        }
    }
}

enum InternalFileExistence<F: 'static> {
    Unloaded,
    Parsed(F),
}

impl<F> fmt::Debug for InternalFileExistence<F> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Intentionally remove the T here, because it's usually huge and we are usually not
        // interested in that while debugging.
        match *self {
            Self::Unloaded => write!(f, "Unloaded"),
            Self::Parsed(_) => write!(f, "Parsed(_)"),
        }
    }
}
