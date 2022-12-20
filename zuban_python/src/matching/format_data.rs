use super::Matcher;
use crate::database::{Database, FormatStyle, RecursiveAlias, TypeVarLikeUsage};

#[derive(Clone, Copy)]
struct DisplayedRecursive<'a> {
    current: &'a RecursiveAlias,
    parent: Option<&'a DisplayedRecursive<'a>>,
}

impl DisplayedRecursive<'_> {
    fn has_already_seen_recursive_alias(&self, rec: &RecursiveAlias) -> bool {
        self.current == rec
            || self
                .parent
                .map(|d| d.has_already_seen_recursive_alias(rec))
                .unwrap_or(false)
    }
}

#[derive(Clone, Copy)]
pub enum ParamsStyle {
    FunctionParams,
    CallableParamsInner,
    CallableParams,
    Unreachable,
}

pub struct FormatData<'db, 'a, 'b, 'c> {
    pub db: &'db Database,
    matcher: Option<&'b Matcher<'a>>,
    pub style: FormatStyle,
    pub verbose: bool,
    displayed_recursive: Option<DisplayedRecursive<'c>>,
}

impl<'db, 'a, 'b, 'c> FormatData<'db, 'a, 'b, 'c> {
    pub fn new_short(db: &'db Database) -> Self {
        Self {
            db,
            matcher: None,
            style: FormatStyle::Short,
            verbose: false,
            displayed_recursive: None,
        }
    }

    pub fn with_style(db: &'db Database, style: FormatStyle) -> Self {
        Self {
            db,
            matcher: None,
            style,
            verbose: false,
            displayed_recursive: None,
        }
    }

    pub fn with_matcher(db: &'db Database, matcher: &'b Matcher<'a>) -> Self {
        Self {
            db,
            matcher: Some(matcher),
            style: FormatStyle::Short,
            verbose: false,
            displayed_recursive: None,
        }
    }

    pub fn with_matcher_and_style(
        db: &'db Database,
        matcher: &'b Matcher<'a>,
        style: FormatStyle,
    ) -> Self {
        Self {
            db,
            matcher: Some(matcher),
            style,
            verbose: false,
            displayed_recursive: None,
        }
    }

    pub fn with_seen_recursive_alias<'x: 'c>(
        &'x self,
        rec: &'x RecursiveAlias,
    ) -> FormatData<'db, 'a, 'b, 'x> {
        Self {
            db: self.db,
            matcher: self.matcher,
            style: self.style,
            verbose: self.verbose,
            displayed_recursive: Some(DisplayedRecursive {
                current: rec,
                parent: self.displayed_recursive.as_ref(),
            }),
        }
    }

    pub fn remove_matcher<'x: 'c>(&'x self) -> Self {
        Self {
            db: self.db,
            matcher: None,
            style: self.style,
            verbose: self.verbose,
            displayed_recursive: self.displayed_recursive,
        }
    }

    pub fn has_already_seen_recursive_alias(&self, rec: &RecursiveAlias) -> bool {
        if let Some(displayed_recursive) = &self.displayed_recursive {
            displayed_recursive.has_already_seen_recursive_alias(rec)
        } else {
            false
        }
    }

    pub fn enable_verbose(&mut self) {
        self.verbose = true;
    }

    pub fn format_type_var_like(
        &self,
        type_var_usage: &TypeVarLikeUsage,
        style: ParamsStyle,
    ) -> Box<str> {
        if let Some(matcher) = self.matcher {
            if matcher.has_type_var_matcher() {
                return matcher.format_in_type_var_matcher(type_var_usage, self, style);
            }
        }
        type_var_usage.format_without_matcher(self.db, self.style, style)
    }
}
