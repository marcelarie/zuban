use super::super::{FormatData, Match, Type};
use crate::database::{Database, DbType, FormatStyle, Variance};
use crate::inference_state::InferenceState;

#[derive(Debug, Clone)]
pub enum TypeVarBound {
    Invariant(DbType),
    Lower(DbType),
    LowerAndUpper(DbType, DbType),
    Upper(DbType),
}

impl TypeVarBound {
    pub fn new(t: DbType, variance: Variance) -> Self {
        match variance {
            Variance::Invariant => Self::Invariant(t),
            Variance::Covariant => Self::Upper(t),
            Variance::Contravariant => Self::Lower(t),
        }
    }

    pub fn invert_bounds(&mut self) {
        match std::mem::replace(self, Self::Invariant(DbType::Any)) {
            Self::Invariant(t) => *self = Self::Invariant(t),
            Self::Upper(t) => *self = Self::Lower(t),
            Self::Lower(t) => *self = Self::Upper(t),
            Self::LowerAndUpper(_, _) => unreachable!(),
        }
    }

    pub fn format(&self, db: &Database, style: FormatStyle) -> Box<str> {
        match self {
            Self::Invariant(t) | Self::Lower(t) | Self::Upper(t) | Self::LowerAndUpper(t, _) => {
                t.format(&FormatData::with_style(db, style))
            }
        }
    }

    pub fn into_db_type(self, db: &Database) -> DbType {
        match self {
            // If the lower bound is a literal, we do not want to lower the bound.
            Self::LowerAndUpper(t @ DbType::Literal(_), _) => t,
            Self::Upper(DbType::Literal(l)) | Self::LowerAndUpper(_, DbType::Literal(l))
                if l.implicit =>
            {
                db.python_state.literal_db_type(l.kind)
            }
            Self::Invariant(t) | Self::Lower(t) | Self::Upper(t) | Self::LowerAndUpper(t, _) => t,
        }
    }

    fn update_lower_bound(&mut self, lower: DbType) {
        match self {
            Self::Lower(_) => *self = Self::Lower(lower),
            Self::Upper(upper) | Self::LowerAndUpper(_, upper) => {
                *self = Self::LowerAndUpper(lower, upper.clone())
            }
            Self::Invariant(_) => unreachable!(),
        }
    }

    fn update_upper_bound(&mut self, upper: DbType) {
        match self {
            Self::Upper(_) => *self = Self::Upper(upper),
            Self::Lower(lower) | Self::LowerAndUpper(lower, _) => {
                *self = Self::LowerAndUpper(lower.clone(), upper)
            }
            Self::Invariant(_) => unreachable!(),
        }
    }

    pub fn merge_or_mismatch<'db>(
        &mut self,
        i_s: &mut InferenceState<'db, '_>,
        other: &Type,
        variance: Variance,
    ) -> Match {
        // First check if the value is between the bounds.
        let matches = match self {
            Self::Invariant(t) => {
                let m = Type::new(t).is_simple_same_type(i_s, other);
                if m.bool() {
                    return m; // In the false case we still have to check for the variance cases.
                }
                m
            }
            Self::Lower(lower) => Type::new(lower).is_simple_super_type_of(i_s, other),
            Self::Upper(upper) => Type::new(upper).is_simple_sub_type_of(i_s, other),
            Self::LowerAndUpper(lower, upper) => {
                Type::new(lower).is_simple_super_type_of(i_s, other)
                    & Type::new(upper).is_simple_sub_type_of(i_s, other)
            }
        };
        if matches.bool() {
            // If we are between the bounds we might need to update lower/upper bounds
            let db_other = other.as_db_type(i_s.db);
            match variance {
                Variance::Invariant => *self = Self::Invariant(db_other),
                Variance::Covariant => self.update_upper_bound(db_other),
                Variance::Contravariant => self.update_lower_bound(db_other),
            }
        } else {
            // If we are not between the lower and upper bound, but the value is co or
            // contravariant, it can still be valid.
            match variance {
                Variance::Invariant => (),
                Variance::Covariant => {
                    if let Self::Invariant(ref t)
                    | Self::Upper(ref t)
                    | Self::LowerAndUpper(_, ref t) = self
                    {
                        let m = Type::new(t).is_simple_super_type_of(i_s, other);
                        if !m.bool() && matches!(self, Self::Upper(_)) {
                            *self = Self::Upper(Type::new(t).common_base_class(i_s, other));
                            return Match::new_true();
                        }
                        return m;
                    }
                }
                Variance::Contravariant => {
                    if let Self::Invariant(t) | Self::Lower(t) | Self::LowerAndUpper(t, _) = self {
                        return Type::new(t).is_simple_sub_type_of(i_s, other);
                    }
                }
            };
        }
        matches
    }
}
