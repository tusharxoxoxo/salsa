#![deny(rust_2018_idioms)]
#![allow(dead_code)]
#![allow(unused_imports)]

use derive_new::new;
use rustc_hash::FxHashMap;
use std::any::Any;
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::fmt::Debug;
use std::fmt::Display;
use std::fmt::Write;
use std::hash::Hash;

pub mod derived;
pub mod input;
pub mod runtime;

pub use crate::runtime::Runtime;

/// The base trait which your "query context" must implement. Gives
/// access to the salsa runtime, which you must embed into your query
/// context (along with whatever other state you may require).
pub trait Database: DatabaseStorageTypes {
    /// Gives access to the underlying salsa runtime.
    fn salsa_runtime(&self) -> &Runtime<Self>;

    /// Get access to extra methods pertaining to a given query,
    /// notably `set` (for inputs).
    #[allow(unused_variables)]
    fn query<Q>(&self, query: Q) -> QueryTable<'_, Self, Q>
    where
        Q: Query<Self>,
        Self: GetQueryTable<Q>,
    {
        <Self as GetQueryTable<Q>>::get_query_table(self)
    }
}

/// Defines the `QueryDescriptor` associated type. An impl of this
/// should be generated for your query-context type automatically by
/// the `database_storage` macro, so you shouldn't need to mess
/// with this trait directly.
pub trait DatabaseStorageTypes: Sized {
    /// A "query descriptor" packages up all the possible queries and a key.
    /// It is used to store information about (e.g.) the stack.
    ///
    /// At runtime, it can be implemented in various ways: a monster enum
    /// works for a fixed set of queries, but a boxed trait object is good
    /// for a more open-ended option.
    type QueryDescriptor: QueryDescriptor<Self>;

    /// Defines the "storage type", where all the query data is kept.
    /// This type is defined by the `database_storage` macro.
    type DatabaseStorage: Default;
}

pub trait QueryDescriptor<DB>: Clone + Debug + Eq + Hash + Send + Sync {
    /// Returns true if the value of this query may have changed since
    /// the given revision.
    fn maybe_changed_since(&self, db: &DB, revision: runtime::Revision) -> bool;
}

pub trait QueryFunction<DB: Database>: Query<DB> {
    fn execute(db: &DB, key: Self::Key) -> Self::Value;
}

pub trait Query<DB: Database>: Debug + Default + Sized + 'static {
    type Key: Clone + Debug + Hash + Eq;
    type Value: Clone + Debug + Hash + Eq;
    type Storage: QueryStorageOps<DB, Self> + Send + Sync;
}

pub trait GetQueryTable<Q: Query<Self>>: Database {
    fn get_query_table(db: &Self) -> QueryTable<'_, Self, Q>;
}

pub trait QueryStorageOps<DB, Q>: Default
where
    DB: Database,
    Q: Query<DB>,
{
    /// Execute the query, returning the result (often, the result
    /// will be memoized).  This is the "main method" for
    /// queries.
    ///
    /// Returns `Err` in the event of a cycle, meaning that computing
    /// the value for this `key` is recursively attempting to fetch
    /// itself.
    fn try_fetch(
        &self,
        db: &DB,
        key: &Q::Key,
        descriptor: &DB::QueryDescriptor,
    ) -> Result<Q::Value, CycleDetected>;

    /// True if the query **may** have changed since the given
    /// revision. The query will answer this question with as much
    /// precision as it is able to do based on its storage type.  In
    /// the event of a cycle being detected as part of this function,
    /// it returns true.
    ///
    /// Example: The steps for a memoized query are as follows.
    ///
    /// - If the query has already been computed:
    ///   - Check the inputs that the previous computation used
    ///     recursively to see if *they* have changed.  If they have
    ///     not, then return false.
    ///   - If they have, then the query is re-executed and the new
    ///     result is compared against the old result. If it is equal,
    ///     then return false.
    /// - Return true.
    ///
    /// Other storage types will skip some or all of these steps.
    fn maybe_changed_since(
        &self,
        db: &DB,
        revision: runtime::Revision,
        key: &Q::Key,
        descriptor: &DB::QueryDescriptor,
    ) -> bool;
}

/// An optional trait that is implemented for "user mutable" storage:
/// that is, storage whose value is not derived from other storage but
/// is set independently.
pub trait InputQueryStorageOps<DB, Q>: Default
where
    DB: Database,
    Q: Query<DB>,
{
    fn set(&self, db: &DB, key: &Q::Key, new_value: Q::Value);
}

/// An optional trait that is implemented for "user mutable" storage:
/// that is, storage whose value is not derived from other storage but
/// is set independently.
pub trait UncheckedMutQueryStorageOps<DB, Q>: Default
where
    DB: Database,
    Q: Query<DB>,
{
    fn set_unchecked(&self, db: &DB, key: &Q::Key, new_value: Q::Value);
}

#[derive(new)]
pub struct QueryTable<'me, DB, Q>
where
    DB: Database,
    Q: Query<DB>,
{
    db: &'me DB,
    storage: &'me Q::Storage,
    descriptor_fn: fn(&DB, &Q::Key) -> DB::QueryDescriptor,
}

pub struct CycleDetected;

impl<DB, Q> QueryTable<'_, DB, Q>
where
    DB: Database,
    Q: Query<DB>,
{
    pub fn get(&self, key: Q::Key) -> Q::Value {
        let descriptor = self.descriptor(&key);
        self.storage
            .try_fetch(self.db, &key, &descriptor)
            .unwrap_or_else(|CycleDetected| {
                self.db.salsa_runtime().report_unexpected_cycle(descriptor)
            })
    }

    /// Assign a value to an "input query". Must be used outside of
    /// an active query computation.
    pub fn set(&self, key: Q::Key, value: Q::Value)
    where
        Q::Storage: InputQueryStorageOps<DB, Q>,
    {
        self.storage.set(self.db, &key, value);
    }

    /// Assigns a value to the query **bypassing the normal
    /// incremental checking** -- this value becomes the value for the
    /// query in the current revision. This can even be used on
    /// "derived" queries (so long as their results are memoized).
    ///
    /// Note that once `set_unchecked` is used, the result is
    /// effectively "fixed" for all future revisions. This "mocking"
    /// system is pretty primitive and subject to revision; see
    /// [salsa-rs/salsa#34](https://github.com/salsa-rs/salsa/issues/34)
    /// for more details.
    ///
    /// **This is only meant to be used for "mocking" purposes in
    /// tests** -- when testing a given query, you can use
    /// `set_unchecked` to assign the values for its various inputs
    /// and thus control what it sees when it executes.
    pub fn set_unchecked(&self, key: Q::Key, value: Q::Value)
    where
        Q::Storage: UncheckedMutQueryStorageOps<DB, Q>,
    {
        self.storage.set_unchecked(self.db, &key, value);
    }

    fn descriptor(&self, key: &Q::Key) -> DB::QueryDescriptor {
        (self.descriptor_fn)(self.db, key)
    }
}

/// A macro that helps in defining the "context trait" of a given
/// module.  This is a trait that defines everything that a block of
/// queries need to execute, as well as defining the queries
/// themselves that are exported for others to use.
///
/// This macro declares the "prototype" for a group of queries. It will
/// expand into a trait and a set of structs, one per query.
///
/// For each query, you give the name of the accessor method to invoke
/// the query (e.g., `my_query`, below), as well as its input/output
/// types.  You also give the name for a query type (e.g., `MyQuery`,
/// below) that represents the query, and optionally other details,
/// such as its storage.
///
/// ### Examples
///
/// The simplest example is something like this:
///
/// ```ignore
/// trait TypeckDatabase {
///     query_group! {
///         /// Comments or other attributes can go here
///         fn my_query(input: u32) -> u64 {
///             type MyQuery;
///             storage memoized; // optional, this is the default
///             use fn path::to::fn; // optional, default is `my_query`
///         }
///     }
/// }
/// ```
#[macro_export]
macro_rules! query_group {
    (
        $(#[$attr:meta])* $v:vis trait $name:ident { $($t:tt)* }
    ) => {
        $crate::query_group! {
            attr[$(#[$attr])*];
            headers[$v, $name, ];
            tokens[{ $($t)* }];
        }
    };

    (
        $(#[$attr:meta])* $v:vis trait $name:ident : $($t:tt)*
    ) => {
        $crate::query_group! {
            attr[$(#[$attr])*];
            headers[$v, $name, ];
            tokens[$($t)*];
        }
    };

    // Base case: found the trait body
    (
        attr[$($trait_attr:tt)*];
        headers[$v:vis, $query_trait:ident, $($header:tt)*];
        tokens[{
            $(
                $(#[$method_attr:meta])*
                fn $method_name:ident($key_name:ident: $key_ty:ty) -> $value_ty:ty {
                    type $QueryType:ident;
                    $(storage $storage:ident;)* // FIXME(rust-lang/rust#48075) should be `?`
                    $(use fn $fn_path:path;)* // FIXME(rust-lang/rust#48075) should be `?`
                }
            )*
        }];
    ) => {
        $($trait_attr)* $v trait $query_trait: $($crate::GetQueryTable<$QueryType> +)* $($header)* {
            $(
                $(#[$method_attr])*
                fn $method_name(&self, key: $key_ty) -> $value_ty {
                    <Self as $crate::GetQueryTable<$QueryType>>::get_query_table(self)
                        .get(key)
                }
            )*
        }

        $(
            #[derive(Default, Debug)]
            $v struct $QueryType;

            impl<DB> $crate::Query<DB> for $QueryType
            where
                DB: $query_trait,
            {
                type Key = $key_ty;
                type Value = $value_ty;
                type Storage = $crate::query_group! { @storage_ty[DB, Self, $($storage)*] };
            }

            $crate::query_group! {
                @query_fn[
                    storage($($storage)*);
                    method_name($method_name);
                    fn_path($($fn_path)*);
                    db_trait($query_trait);
                    query_type($QueryType);
                ]
            }
        )*
    };

    (
        @query_fn[
            storage(input);
            method_name($method_name:ident);
            fn_path();
            $($rest:tt)*
        ]
    ) => {
        // do nothing for `storage input`, presuming they did not write an explicit `use fn`
    };

    (
        @query_fn[
            storage(input);
            method_name($method_name:ident);
            fn_path($fn_path:path);
            $($rest:tt)*
        ]
    ) => {
        // error for `storage input` with an explicit `use fn`
        compile_error! {
            "cannot have `storage input` combined with `use fn`"
        }
    };

    (
        @query_fn[
            storage($($storage:ident)*);
            method_name($method_name:ident);
            fn_path();
            $($rest:tt)*
        ]
    ) => {
        // default to `use fn $method_name`
        $crate::query_group! {
            @query_fn[
                storage($($storage)*);
                method_name($method_name);
                fn_path($method_name);
                $($rest)*
            ]
        }
    };

    (
        @query_fn[
            storage($($storage:ident)*);
            method_name($method_name:ident);
            fn_path($fn_path:path);
            db_trait($DbTrait:path);
            query_type($QueryType:ty);
        ]
    ) => {
        impl<DB> $crate::QueryFunction<DB> for $QueryType
        where DB: $DbTrait
        {
            fn execute(db: &DB, key: <Self as $crate::Query<DB>>::Key)
                       -> <Self as $crate::Query<DB>>::Value
            {
                $fn_path(db, key)
            }
        }
    };

    // Recursive case: found some more part of the trait header.
    // Keep pulling out tokens until we find the body.
    (
        attr[$($attr:tt)*];
        headers[$($headers:tt)*];
        tokens[$token:tt $($tokens:tt)*];
    ) => {
        $crate::query_group! {
            attr[$($attr)*];
            headers[$($headers)* $token];
            tokens[$($tokens)*];
        }
    };

    // Generate storage type
    (
        // Default case:
        @storage_ty[$DB:ident, $Self:ident, ]
    ) => {
        $crate::query_group! { @storage_ty[$DB, $Self, memoized] }
    };

    (
        @storage_ty[$DB:ident, $Self:ident, memoized]
    ) => {
        $crate::derived::MemoizedStorage<$DB, $Self>
    };

    (
        @storage_ty[$DB:ident, $Self:ident, volatile]
    ) => {
        $crate::derived::VolatileStorage<$DB, $Self>
    };

    (
        @storage_ty[$DB:ident, $Self:ident, dependencies]
    ) => {
        $crate::derived::DependencyStorage<$DB, $Self>
    };

    (
        @storage_ty[$DB:ident, $Self:ident, input]
    ) => {
        $crate::input::InputStorage<DB, Self>
    };
}

/// This macro generates the "query storage" that goes into your query
/// context.
#[macro_export]
macro_rules! database_storage {
    (
        $(#[$attr:meta])*
        $v:vis struct $Storage:ident for $Database:ty {
            $(
                impl $TraitName:path {
                    $(
                        fn $query_method:ident() for $QueryType:path;
                    )*
                }
            )*
        }
    ) => {
        #[derive(Default)]
        $(#[$attr])*
        $v struct $Storage {
            $(
                $(
                    $query_method: <$QueryType as $crate::Query<$Database>>::Storage,
                )*
            )*
        }

        /// Identifies a query and its key. You are not meant to name
        /// this type directly or use its fields etc.  It is a
        /// **private query descriptor type generated by salsa** and
        /// its exact structure is subject to change. Sadly, I don't
        /// know any way to hide this with hygiene, so use `__`
        /// instead.
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        $v struct __SalsaQueryDescriptor {
            kind: __SalsaQueryDescriptorKind
        }

        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        enum __SalsaQueryDescriptorKind {
            $(
                $(
                    $query_method(<$QueryType as $crate::Query<$Database>>::Key),
                )*
            )*
        }

        impl $crate::DatabaseStorageTypes for $Database {
            type QueryDescriptor = __SalsaQueryDescriptor;
            type DatabaseStorage = $Storage;
        }

        impl $crate::QueryDescriptor<$Database> for __SalsaQueryDescriptor {
            fn maybe_changed_since(
                &self,
                db: &$Database,
                revision: $crate::runtime::Revision,
            ) -> bool {
                match &self.kind {
                    $(
                        $(
                            __SalsaQueryDescriptorKind::$query_method(key) => {
                                let runtime = $crate::Database::salsa_runtime(db);
                                let storage = &runtime.storage().$query_method;
                                <_ as $crate::QueryStorageOps<$Database, $QueryType>>::maybe_changed_since(
                                    storage,
                                    db,
                                    revision,
                                    key,
                                    self,
                                )
                            }
                        )*
                    )*
                }
            }
        }

        $(
            impl $TraitName for $Database { }

            $(
                impl $crate::GetQueryTable<$QueryType> for $Database {
                    fn get_query_table(
                        db: &Self,
                    ) -> $crate::QueryTable<'_, Self, $QueryType> {
                        $crate::QueryTable::new(
                            db,
                            &$crate::Database::salsa_runtime(db)
                                .storage()
                                .$query_method,
                            |_, key| {
                                let key = std::clone::Clone::clone(key);
                                __SalsaQueryDescriptor {
                                    kind: __SalsaQueryDescriptorKind::$query_method(key),
                                }
                            },
                        )
                    }
                }
            )*
        )*
    };
}
