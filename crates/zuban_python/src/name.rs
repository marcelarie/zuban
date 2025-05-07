#![allow(dead_code)] // TODO remove this

use std::fmt;

use parsa_python_cst::Name as CSTName;

use crate::{
    database::Database,
    file::{File, PythonFile},
};

type Signatures = Vec<()>;
pub type Names<'db> = Vec<Box<dyn Name<'db>>>;

pub trait Name<'db>: fmt::Debug {
    fn name(&self) -> &str;

    fn file_path(&self) -> &str;

    // TODO
    //fn definition_start_and_end_position(&self) -> (TreePosition, TreePosition);

    fn documentation(&self) -> String;

    fn description(&self) -> String;

    fn qualified_names(&self) -> Option<Vec<String>>;

    fn is_implementation(&self) -> bool {
        true
    }

    fn type_hint(&self) -> Option<String> {
        None
    }

    fn signatures(&self) -> Signatures {
        vec![]
    }

    fn infer(&self);

    fn goto(&self) -> Names<'db>;

    fn is_definition(&self) -> bool {
        false
    }
}

pub(crate) struct TreeName<'db, F: File, N> {
    db: &'db Database,
    file: &'db F,
    cst_name: N,
}

impl<'db> fmt::Debug for TreeName<'db, PythonFile, CSTName<'db>> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("TreeName")
            .field("file", &self.file_path())
            .field("name", &self.name())
            .finish()
    }
}

impl<'db, F: File, N> TreeName<'db, F, N> {
    pub fn new(db: &'db Database, file: &'db F, cst_name: N) -> Self {
        Self { db, cst_name, file }
    }
}

impl<'db> Name<'db> for TreeName<'db, PythonFile, CSTName<'db>> {
    fn name(&self) -> &str {
        self.cst_name.as_str()
    }

    fn file_path(&self) -> &str {
        self.db.file_path(self.file.file_index)
    }

    fn documentation(&self) -> String {
        unimplemented!()
    }

    fn description(&self) -> String {
        unimplemented!()
    }

    fn qualified_names(&self) -> Option<Vec<String>> {
        unimplemented!()
    }

    /*
    fn is_implementation(&self) {
    }
    */

    fn infer(&self) {
        /*
        let i_s = InferenceState::new(self.db);
        self.file
            .inference(&i_s)
            .infer_name_of_definition(self.cst_name);
        */
        // TODO
    }

    fn goto(&self) -> Names<'db> {
        unimplemented!()
    }
}

/*
struct WithValueName<'db, 'a, 'b> {
    db: &'db Database,
    value: &'b dyn Value<'db, 'a>,
}

impl<'db, 'a, 'b> WithValueName<'db, 'a, 'b> {
    pub fn new(db: &'db Database, value: &'b dyn Value<'db, 'a>) -> Self {
        Self { db, value }
    }
}

impl fmt::Debug for WithValueName<'_, '_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("WithValueName")
            .field("value", &self.value)
            .finish()
    }
}

impl<'db> Name<'db> for WithValueName<'db, '_, '_> {
    fn name(&self) -> &str {
        unimplemented!()
        //self.value.name()
    }

    fn file_path(&self) -> &str {
        unimplemented!()
        //self.value.file().path()
    }

    fn start_position(&self) -> TreePosition<'db> {
        unimplemented!()
        //TreePosition {file: self.value.file(), position: unimplemented!()}
    }

    fn end_position(&self) -> TreePosition<'db> {
        unimplemented!()
        //TreePosition {file: self.value.file(), position: unimplemented!()}
    }

    fn documentation(&self) -> String {
        unimplemented!()
    }

    fn description(&self) -> String {
        unimplemented!()
    }

    fn qualified_names(&self) -> Option<Vec<String>> {
        unimplemented!()
    }

    fn infer(&self) -> Inferred {
        unimplemented!()
    }

    fn goto(&self) -> Names<'db> {
        unimplemented!()
    }

    /*
    fn is_implementation(&self) {
    }
    */
}

enum ValueNameIterator<T> {
    Single(T),
    Multiple(Vec<T>),
    Finished,
}

impl<T> Iterator for ValueNameIterator<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Single(t) => {
                let result = mem::replace(self, Self::Finished);
                // Is this really the best way to do this? Please tell me!!!
                if let Self::Single(t) = result {
                    Some(t)
                } else {
                    unreachable!()
                }
            }
            Self::Multiple(list) => list.pop(),
            Self::Finished => None,
        }
    }
}
*/
