use crossbeam::atomic::AtomicCell;
use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;

use crate::durability::Durability;
use crate::id::AsId;
use crate::ingredient::{fmt_index, IngredientRequiresReset};
use crate::key::DependencyIndex;
use crate::runtime::local_state::QueryOrigin;
use crate::runtime::Runtime;
use crate::{DatabaseKeyIndex, Id};

use super::hash::FxDashMap;
use super::ingredient::Ingredient;
use super::routes::IngredientIndex;
use super::Revision;

pub trait Configuration {
    type Data<'db>: InternedData;
}

pub trait InternedData: Sized + Eq + Hash + Clone {}
impl<T: Eq + Hash + Clone> InternedData for T {}

/// The interned ingredient has the job of hashing values of type `Data` to produce an `Id`.
/// It used to store interned structs but also to store the id fields of a tracked struct.
/// Interned values endure until they are explicitly removed in some way.
pub struct InternedIngredient<C: Configuration> {
    /// Index of this ingredient in the database (used to construct database-ids, etc).
    ingredient_index: IngredientIndex,

    /// Maps from data to the existing interned id for that data.
    ///
    /// Deadlock requirement: We access `value_map` while holding lock on `key_map`, but not vice versa.
    key_map: FxDashMap<C::Data<'static>, Id>,

    /// Maps from an interned id to its data.
    ///
    /// Deadlock requirement: We access `value_map` while holding lock on `key_map`, but not vice versa.
    value_map: FxDashMap<Id, Box<C::Data<'static>>>,

    /// counter for the next id.
    counter: AtomicCell<u32>,

    /// Stores the revision when this interned ingredient was last cleared.
    /// You can clear an interned table at any point, deleting all its entries,
    /// but that will make anything dependent on those entries dirty and in need
    /// of being recomputed.
    reset_at: Revision,

    debug_name: &'static str,
}

impl<C> InternedIngredient<C>
where
    C: Configuration,
{
    pub fn new(ingredient_index: IngredientIndex, debug_name: &'static str) -> Self {
        Self {
            ingredient_index,
            key_map: Default::default(),
            value_map: Default::default(),
            counter: AtomicCell::default(),
            reset_at: Revision::start(),
            debug_name,
        }
    }

    unsafe fn to_internal_data<'db>(&'db self, data: C::Data<'db>) -> C::Data<'static> {
        unsafe { std::mem::transmute(data) }
    }

    unsafe fn from_internal_data<'db>(&'db self, data: &C::Data<'static>) -> &'db C::Data<'db> {
        unsafe { std::mem::transmute(data) }
    }

    /// Intern data to a unique id.
    pub fn intern<'db>(&'db self, runtime: &'db Runtime, data: C::Data<'db>) -> Id {
        runtime.report_tracked_read(
            DependencyIndex::for_table(self.ingredient_index),
            Durability::MAX,
            self.reset_at,
        );

        // Optimisation to only get read lock on the map if the data has already
        // been interned.
        let internal_data = unsafe { self.to_internal_data(data) };
        if let Some(id) = self.key_map.get(&internal_data) {
            return *id;
        }

        match self.key_map.entry(internal_data.clone()) {
            // Data has been interned by a racing call, use that ID instead
            dashmap::mapref::entry::Entry::Occupied(entry) => *entry.get(),

            // We won any races so should intern the data
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let next_id = self.counter.fetch_add(1);
                let next_id = Id::from_id(crate::id::Id::from_u32(next_id));
                let old_value = self.value_map.insert(next_id, Box::new(internal_data));
                assert!(
                    old_value.is_none(),
                    "next_id is guaranteed to be unique, bar overflow"
                );
                entry.insert(next_id);
                next_id
            }
        }
    }

    pub fn reset(&mut self, revision: Revision) {
        assert!(revision > self.reset_at);
        self.reset_at = revision;
        self.key_map.clear();
        self.value_map.clear();
    }

    #[track_caller]
    pub fn data<'db>(&'db self, runtime: &'db Runtime, id: Id) -> &'db C::Data<'db> {
        runtime.report_tracked_read(
            DependencyIndex::for_table(self.ingredient_index),
            Durability::MAX,
            self.reset_at,
        );

        let data = match self.value_map.get(&id) {
            Some(d) => d,
            None => {
                panic!("no data found for id `{:?}`", id)
            }
        };

        // Unsafety clause:
        //
        // * Values are only removed or altered when we have `&mut self`
        unsafe { self.from_internal_data(&data) }
    }
}

impl<DB: ?Sized, C> Ingredient<DB> for InternedIngredient<C>
where
    C: Configuration,
{
    fn ingredient_index(&self) -> IngredientIndex {
        self.ingredient_index
    }

    fn maybe_changed_after(&self, _db: &DB, _input: DependencyIndex, revision: Revision) -> bool {
        revision < self.reset_at
    }

    fn cycle_recovery_strategy(&self) -> crate::cycle::CycleRecoveryStrategy {
        crate::cycle::CycleRecoveryStrategy::Panic
    }

    fn origin(&self, _key_index: crate::Id) -> Option<QueryOrigin> {
        None
    }

    fn mark_validated_output(
        &self,
        _db: &DB,
        executor: DatabaseKeyIndex,
        output_key: Option<crate::Id>,
    ) {
        unreachable!(
            "mark_validated_output({:?}, {:?}): input cannot be the output of a tracked function",
            executor, output_key
        );
    }

    fn remove_stale_output(
        &self,
        _db: &DB,
        executor: DatabaseKeyIndex,
        stale_output_key: Option<crate::Id>,
    ) {
        unreachable!(
            "remove_stale_output({:?}, {:?}): interned ids are not outputs",
            executor, stale_output_key
        );
    }

    fn reset_for_new_revision(&mut self) {
        // Interned ingredients do not, normally, get deleted except when they are "reset" en masse.
        // There ARE methods (e.g., `clear_deleted_entries` and `remove`) for deleting individual
        // items, but those are only used for tracked struct ingredients.
        panic!("unexpected call to `reset_for_new_revision`")
    }

    fn salsa_struct_deleted(&self, _db: &DB, _id: crate::Id) {
        panic!("unexpected call: interned ingredients do not register for salsa struct deletion events");
    }

    fn fmt_index(&self, index: Option<crate::Id>, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_index(self.debug_name, index, fmt)
    }
}

impl<C> IngredientRequiresReset for InternedIngredient<C>
where
    C: Configuration,
{
    const RESET_ON_NEW_REVISION: bool = false;
}

pub struct IdentityInterner<C>
where
    C: Configuration,
    for<'db> C::Data<'db>: AsId,
{
    data: PhantomData<C>,
}

impl<C> IdentityInterner<C>
where
    C: Configuration,
    for<'db> C::Data<'db>: AsId,
{
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        IdentityInterner { data: PhantomData }
    }

    pub fn intern<'db>(&'db self, _runtime: &'db Runtime, id: C::Data<'db>) -> crate::Id {
        id.as_id()
    }

    pub fn data<'db>(&'db self, _runtime: &'db Runtime, id: crate::Id) -> C::Data<'db> {
        <C::Data<'db>>::from_id(id)
    }
}
