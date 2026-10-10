use alloc::vec::Vec;
use core::fmt;

use crate::lex::LexError;
use crate::object::{parse_object, Object, ObjectKind, PdfError};
use crate::source::PdfSource;
use crate::work::{ObjectRef, ObjectWork, WorkBudget, WorkError, WorkLimit};

/// Looks up one indirect object's byte range without parsing other objects.
pub trait ObjectWorkLookup {
    fn object_work(&self, object_id: ObjectRef) -> Option<ObjectWork>;
}

impl<F> ObjectWorkLookup for F
where
    F: Fn(ObjectRef) -> Option<ObjectWork>,
{
    fn object_work(&self, object_id: ObjectRef) -> Option<ObjectWork> {
        self(object_id)
    }
}

/// A caller-supplied bound on the number of indirect objects resolved.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ResolveLimits {
    max_objects: usize,
}

impl ResolveLimits {
    pub const fn new(max_objects: usize) -> Self {
        Self { max_objects }
    }

    pub const fn max_objects(self) -> usize {
        self.max_objects
    }
}

/// The objects reachable from one explicitly requested indirect reference.
///
/// References remain represented by their object IDs inside each parsed object.
/// This collection holds each reachable target once so callers can inspect or
/// resolve a target on demand without parsing unrelated xref entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedObjects<'source> {
    root: ObjectRef,
    objects: Vec<Object<'source>>,
}

impl<'source> ResolvedObjects<'source> {
    pub const fn root_id(&self) -> ObjectRef {
        self.root
    }

    pub fn root(&self) -> Option<&Object<'source>> {
        self.get(self.root)
    }

    pub fn get(&self, object_id: ObjectRef) -> Option<&Object<'source>> {
        self.objects.iter().find(|object| object.id() == object_id)
    }

    pub fn objects(&self) -> &[Object<'source>] {
        &self.objects
    }
}

/// A typed failure while following an explicitly requested object graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveError {
    ObjectNotFound {
        object_id: ObjectRef,
    },
    ObjectIdMismatch {
        expected: ObjectRef,
        found: ObjectRef,
    },
    ReferenceCycle {
        object_id: ObjectRef,
    },
    DepthLimitExceeded {
        object_id: ObjectRef,
        maximum: usize,
    },
    ObjectBudgetExceeded {
        object_id: ObjectRef,
        maximum: usize,
    },
    BudgetExceeded {
        object_id: ObjectRef,
        limit: WorkLimit,
        requested: usize,
        maximum: usize,
    },
    Parse(PdfError),
    TraversalState {
        object_id: ObjectRef,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObjectNotFound { object_id } => write!(
                formatter,
                "object {} {} was not found in the xref lookup",
                object_id.number(),
                object_id.generation()
            ),
            Self::ObjectIdMismatch { expected, found } => write!(
                formatter,
                "xref identifies object {} {}, but lookup returned object {} {}",
                expected.number(),
                expected.generation(),
                found.number(),
                found.generation()
            ),
            Self::ReferenceCycle { object_id } => write!(
                formatter,
                "indirect reference cycle reaches object {} {}",
                object_id.number(),
                object_id.generation()
            ),
            Self::DepthLimitExceeded { object_id, maximum } => write!(
                formatter,
                "reference to object {} {} exceeds depth limit {maximum}",
                object_id.number(),
                object_id.generation()
            ),
            Self::ObjectBudgetExceeded { object_id, maximum } => write!(
                formatter,
                "resolving object {} {} exceeds the object limit {maximum}",
                object_id.number(),
                object_id.generation()
            ),
            Self::BudgetExceeded {
                object_id,
                limit,
                requested,
                maximum,
            } => write!(
                formatter,
                "object {} {} requests {requested} {limit}, exceeding the remaining limit {maximum}",
                object_id.number(),
                object_id.generation()
            ),
            Self::Parse(error) => fmt::Display::fmt(error, formatter),
            Self::TraversalState { object_id } => write!(
                formatter,
                "reference traversal state was inconsistent at object {} {}",
                object_id.number(),
                object_id.generation()
            ),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ResolveError {}

enum Visit {
    Enter { object_id: ObjectRef, depth: usize },
    Exit { object_id: ObjectRef },
}

/// Resolves only the transitive references reachable from `root`.
///
/// The xref lookup is called only for discovered object IDs. Each object is
/// parsed independently at its own xref offset, and no document-wide parse
/// cursor or shared mutable resolver state is retained.
pub fn resolve<'source, L>(
    source: &'source PdfSource,
    root: ObjectRef,
    lookup: &L,
    budget: &WorkBudget,
    limits: ResolveLimits,
) -> Result<ResolvedObjects<'source>, ResolveError>
where
    L: ObjectWorkLookup + ?Sized,
{
    let mut visits = Vec::new();
    let mut active = Vec::new();
    let mut objects = Vec::new();
    let mut input_used = 0usize;
    let mut tokens_used = 0usize;
    visits.push(Visit::Enter {
        object_id: root,
        depth: 0,
    });

    while let Some(visit) = visits.pop() {
        match visit {
            Visit::Exit { object_id } => match active.pop() {
                Some(active_id) if active_id == object_id => {}
                _ => return Err(ResolveError::TraversalState { object_id }),
            },
            Visit::Enter { object_id, depth } => {
                if active.contains(&object_id) {
                    return Err(ResolveError::ReferenceCycle { object_id });
                }
                if objects.iter().any(|object: &Object<'source>| object.id() == object_id) {
                    continue;
                }
                if budget.check_depth(depth).is_err() {
                    return Err(ResolveError::DepthLimitExceeded {
                        object_id,
                        maximum: budget.max_depth(),
                    });
                }
                if objects.len() >= limits.max_objects() {
                    return Err(ResolveError::ObjectBudgetExceeded {
                        object_id,
                        maximum: limits.max_objects(),
                    });
                }

                let work = lookup
                    .object_work(object_id)
                    .ok_or(ResolveError::ObjectNotFound { object_id })?;
                let found_id = work.id();
                if found_id != object_id {
                    return Err(ResolveError::ObjectIdMismatch {
                        expected: object_id,
                        found: found_id,
                    });
                }
                let range = work.candidate_range();
                let requested_input = range.end.checked_sub(range.start).ok_or_else(|| {
                    ResolveError::Parse(PdfError::Work(WorkError::InvertedRange {
                        start: range.start,
                        end: range.end,
                    }))
                })?;
                let remaining_input = budget.max_input_bytes().saturating_sub(input_used);
                let remaining_tokens = budget.max_tokens().saturating_sub(tokens_used);
                let object_budget = WorkBudget::new(
                    remaining_input,
                    remaining_tokens,
                    budget.max_depth(),
                    budget.max_output_bytes(),
                    budget.max_operators(),
                );
                if requested_input > remaining_input {
                    return Err(ResolveError::BudgetExceeded {
                        object_id,
                        limit: WorkLimit::InputBytes,
                        requested: requested_input,
                        maximum: remaining_input,
                    });
                }
                let object = parse_object(source, work, &object_budget)
                    .map_err(|error| {
                        map_parse_error(
                            object_id,
                            error,
                            remaining_input,
                            remaining_tokens,
                            budget.max_depth(),
                        )
                    })?;

                let next_input = input_used.saturating_add(requested_input);
                let next_tokens = tokens_used.saturating_add(object.token_count());
                if next_input > budget.max_input_bytes() {
                    return Err(ResolveError::BudgetExceeded {
                        object_id,
                        limit: WorkLimit::InputBytes,
                        requested: next_input,
                        maximum: budget.max_input_bytes(),
                    });
                }
                if next_tokens > budget.max_tokens() {
                    return Err(ResolveError::BudgetExceeded {
                        object_id,
                        limit: WorkLimit::Tokens,
                        requested: next_tokens,
                        maximum: budget.max_tokens(),
                    });
                }

                let mut dependencies = Vec::new();
                for value in object.values() {
                    if let ObjectKind::Reference(dependency) = value.kind() {
                        dependencies.push(*dependency);
                    }
                }
                objects.push(object);
                active.push(object_id);
                visits.push(Visit::Exit { object_id });
                let Some(dependency_depth) = depth.checked_add(1) else {
                    return Err(ResolveError::DepthLimitExceeded {
                        object_id,
                        maximum: budget.max_depth(),
                    });
                };
                for dependency in dependencies.into_iter().rev() {
                    visits.push(Visit::Enter {
                        object_id: dependency,
                        depth: dependency_depth,
                    });
                }
                input_used = next_input;
                tokens_used = next_tokens;
            }
        }
    }

    Ok(ResolvedObjects {
        root,
        objects,
    })
}

fn map_parse_error(
    object_id: ObjectRef,
    error: PdfError,
    remaining_input: usize,
    remaining_tokens: usize,
    maximum_depth: usize,
) -> ResolveError {
    match error {
        PdfError::ObjectIdMismatch { expected, found } => {
            ResolveError::ObjectIdMismatch { expected, found }
        }
        PdfError::Work(WorkError::BudgetExceeded {
            limit: WorkLimit::Depth,
            ..
        }) => ResolveError::DepthLimitExceeded {
            object_id,
            maximum: maximum_depth,
        },
        PdfError::Work(WorkError::BudgetExceeded {
            limit,
            requested,
            maximum: _,
        }) => ResolveError::BudgetExceeded {
            object_id,
            limit,
            requested,
            maximum: if limit == WorkLimit::InputBytes {
                remaining_input
            } else {
                remaining_tokens
            },
        },
        PdfError::Lexical {
            error: LexError::TokenBudgetExceeded { limit, .. },
            ..
        } => ResolveError::BudgetExceeded {
            object_id,
            limit: WorkLimit::Tokens,
            requested: limit.saturating_add(1),
            maximum: remaining_tokens,
        },
        PdfError::Lexical {
            error: LexError::LiteralNestingTooDeep { .. },
            ..
        } => ResolveError::DepthLimitExceeded {
            object_id,
            maximum: maximum_depth,
        },
        error => ResolveError::Parse(error),
    }
}
