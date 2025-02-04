use std::{
    borrow::Cow,
    cell::Cell,
    collections::HashMap,
    fmt,
    hash::{Hash, Hasher},
    rc::Rc,
};

use parsa_python_cst::{Name, NodeIndex};

thread_local!(pub static DEBUG_INDENTATION: Cell<usize> = const { Cell::new(0) });

#[inline]
pub fn debug_indent<C: FnOnce() -> T, T>(f: C) -> T {
    if cfg!(feature = "zuban_debug") {
        DEBUG_INDENTATION.with(|i| {
            i.set(i.get() + 1);
            let result = f();
            i.set(i.get() - 1);
            result
        })
    } else {
        f()
    }
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        if cfg!(feature="zuban_debug") {
            use std::iter::repeat;
            let indent = $crate::utils::DEBUG_INDENTATION.with(|i| i.get());
            print!("{}", repeat(' ').take(indent).collect::<String>());
            println!($($arg)*);
        }
    }
}

#[macro_export]
macro_rules! new_class {
    ($link:expr, $($arg:expr),+$(,)?) => {
        $crate::type_::Type::new_class(
            $link,
            $crate::type_::ClassGenerics::List($crate::type_::GenericsList::new_generics(std::rc::Rc::new([
                $($crate::type_::GenericItem::TypeArg($arg)),*
            ])))
        )
    }
}

#[derive(Clone)]
pub struct HashableRawStr {
    ptr: *const str,
}

impl HashableRawStr {
    pub fn new(string: &str) -> Self {
        Self { ptr: string }
    }

    fn as_str(&self) -> &str {
        // This is REALLY unsafe. The user of HashableRawStr is responsible for
        // ensuring that the code part lives longer than this piece.
        unsafe { &*self.ptr }
    }
}

impl Hash for HashableRawStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialEq for HashableRawStr {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for HashableRawStr {}

impl fmt::Debug for HashableRawStr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

#[derive(Debug, Default, Clone)]
pub struct SymbolTable {
    // The name symbol table comes from compiler theory, it's basically a mapping of a name to a
    // pointer. To avoid wasting space, we don't use a pointer here, instead we use the node index,
    // which acts as one.
    symbols: HashMap<HashableRawStr, NodeIndex>,
}

impl SymbolTable {
    pub fn iter(&self) -> impl Iterator<Item = (&str, &NodeIndex)> {
        // This should only ever be called on a table that is not still mutated.
        self.symbols.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn add_or_replace_symbol(&mut self, name: Name) -> Option<NodeIndex> {
        self.symbols
            .insert(HashableRawStr::new(name.as_str()), name.index())
    }

    pub fn lookup_symbol(&self, name: &str) -> Option<NodeIndex> {
        self.symbols.get(&HashableRawStr::new(name)).copied()
    }
}

pub fn bytes_repr(bytes: Cow<[u8]>) -> String {
    let mut string = String::with_capacity(bytes.len());
    for &b in bytes.iter() {
        match b {
            b'\t' => string.push_str(r"\t"),
            b'\n' => string.push_str(r"\n"),
            b'\r' => string.push_str(r"\r"),
            b'\\' => string.push_str(r"\\"),
            b' ' => string.push(' '),
            _ if b.is_ascii_graphic() => string.push(b as char),
            _ => string += &format!("\\x{:02x}", b),
        }
    }
    format!("b'{string}'")
}

pub fn str_repr(content: &str) -> String {
    let mut repr = String::new();
    for c in content.chars() {
        if c.is_ascii_control() {
            match c {
                '\n' => repr += "\\n",
                '\r' => repr += "\\r",
                '\t' => repr += "\\t",
                _ => {
                    repr += "\\";
                    repr += &format!("{:#04x}", c as u8)[1..];
                }
            }
        } else if c == '\\' {
            repr += r"\\";
        } else {
            repr.push(c);
        }
    }
    format!("'{repr}'")
}

pub fn join_with_commas(input: impl Iterator<Item = String>) -> String {
    input.collect::<Vec<_>>().join(", ")
}

pub fn rc_slice_into_vec<T: Clone>(this: Rc<[T]>) -> Vec<T> {
    // Performance issue: Rc -> Vec check https://github.com/rust-lang/rust/issues/93610#issuecomment-1528108612

    // TODO we could avoid cloning here and just use a copy for the slice parts.
    // See also some discussion how this could be done here:
    // https://stackoverflow.com/questions/77511698/rct-try-unwrap-into-vect#comment136989622_77511997
    Vec::from(this.as_ref())
}

pub struct AlreadySeen<'a, T> {
    pub current: T,
    pub previous: Option<&'a AlreadySeen<'a, T>>,
}

impl<T: PartialEq<T>> AlreadySeen<'_, T> {
    pub fn is_cycle(&self) -> bool {
        self.iter_ancestors()
            .any(|ancestor| *ancestor == self.current)
    }
}

impl<'a, T> AlreadySeen<'a, T> {
    pub fn new(current: T) -> Self {
        Self {
            current,
            previous: None,
        }
    }

    pub fn iter_ancestors(&self) -> AlreadySeenIterator<'a, T> {
        AlreadySeenIterator(self.previous)
    }

    pub fn append<'x: 'a>(&'x self, current: T) -> AlreadySeen<'x, T> {
        Self {
            current,
            previous: Some(self),
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for AlreadySeen<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter_ancestors()).finish()
    }
}

impl<T: Clone> Clone for AlreadySeen<'_, T> {
    fn clone(&self) -> Self {
        Self {
            current: self.current.clone(),
            previous: self.previous,
        }
    }
}

impl<T: Copy> Copy for AlreadySeen<'_, T> {}

pub struct AlreadySeenIterator<'a, T>(Option<&'a AlreadySeen<'a, T>>);

impl<'a, T> Iterator for AlreadySeenIterator<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        let first = self.0.take()?;
        let result = Some(&first.current);
        self.0 = first.previous;
        result
    }
}
