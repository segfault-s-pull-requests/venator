use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::{Add, Range};

use ghost_cell::GhostToken;
use input::{FilterPredicate, FilterPropertyKind, FilterValueOperator};
use serde::Deserialize;

use crate::index::{EventIndexes, SpanDurationIndex, SpanIndexes};
use crate::models::{parse_full_span_id, EventKey, Level, SpanKey, Timestamp};
use crate::storage::Storage;
use crate::{Ancestors, Event, InstanceId, InstanceKey, RawEngine, SpanId};

pub mod input;

#[derive(Deserialize)]
pub struct EventQuery {
    pub filter: Vec<FilterPredicate>,
    pub order: Order,
    pub limit: usize,
    pub start: Timestamp,
    pub end: Timestamp,
    // when paginating, this is the last key of the previous call
    pub previous: Option<Timestamp>,
}

pub enum IndexedEventFilter<'i> {
    Single(&'i [Timestamp], Option<NonIndexedEventFilter>),
    And(Vec<IndexedEventFilter<'i>>),
    Or(Vec<IndexedEventFilter<'i>>),
}

impl IndexedEventFilter<'_> {
    pub fn build(
        filter: Option<BasicEventFilter>,
        event_indexes: &EventIndexes,
    ) -> IndexedEventFilter<'_> {
        let Some(filter) = filter else {
            return IndexedEventFilter::Single(&event_indexes.all, None);
        };

        match filter {
            BasicEventFilter::Level(level) => {
                IndexedEventFilter::Single(&event_indexes.levels[level as usize], None)
            }
            BasicEventFilter::Instance(instance_key) => {
                let instance_index = event_indexes
                    .instances
                    .get(&instance_key)
                    .map(Vec::as_slice)
                    .unwrap_or_default();

                IndexedEventFilter::Single(instance_index, None)
            }
            BasicEventFilter::Ancestor(ancestor_key) => {
                let index = &event_indexes.descendents[&ancestor_key];

                IndexedEventFilter::Single(index, None)
            }
            BasicEventFilter::Attribute(attribute, value) => {
                if let Some(attr_index) = event_indexes.attributes.get(&attribute) {
                    let value_index = attr_index
                        .get(&value)
                        .map(Vec::as_slice)
                        .unwrap_or_default();

                    IndexedEventFilter::Single(value_index, None)
                } else {
                    IndexedEventFilter::Single(
                        &event_indexes.all,
                        Some(NonIndexedEventFilter::Attribute(attribute, value)),
                    )
                }
            }
            BasicEventFilter::And(filters) => IndexedEventFilter::And(
                filters
                    .into_iter()
                    .map(|f| IndexedEventFilter::build(Some(f), event_indexes))
                    .collect(),
            ),
            BasicEventFilter::Or(filters) => IndexedEventFilter::Or(
                filters
                    .into_iter()
                    .map(|f| IndexedEventFilter::build(Some(f), event_indexes))
                    .collect(),
            ),
        }
    }

    // This searches for an entry equal to or beyond the provided entry
    pub fn search<'b, S: Storage>(
        &mut self,
        token: &GhostToken<'b>,
        storage: &S,
        event_ancestors: &HashMap<Timestamp, Ancestors<'b>>,
        mut entry: Timestamp,
        order: Order,
        bound: Timestamp,
    ) -> Option<Timestamp> {
        match self {
            IndexedEventFilter::Single(entries, filter) => match order {
                Order::Asc => loop {
                    let idx = entries.lower_bound(&entry);
                    *entries = &entries[idx..];
                    let found_entry = entries.first().cloned();

                    let found_entry = found_entry?;
                    if found_entry > bound {
                        return None;
                    }

                    if let Some(filter) = filter {
                        if filter.matches(token, storage, event_ancestors, found_entry) {
                            return Some(found_entry);
                        } else {
                            entry = found_entry.saturating_add(1);
                        }
                    } else {
                        return Some(found_entry);
                    }
                },
                Order::Desc => loop {
                    let idx = entries.upper_bound(&entry);
                    *entries = &entries[..idx];
                    let found_entry = entries.last().cloned();

                    let found_entry = found_entry?;
                    if found_entry < bound {
                        return None;
                    }

                    if let Some(filter) = filter {
                        if filter.matches(token, storage, event_ancestors, found_entry) {
                            return Some(found_entry);
                        } else {
                            entry = Timestamp::new(found_entry.get() - 1).unwrap();
                        }
                    } else {
                        return Some(found_entry);
                    }
                },
            },
            IndexedEventFilter::And(indexed_filters) => {
                let mut current = entry;
                'outer: loop {
                    current = indexed_filters[0].search(
                        token,
                        storage,
                        event_ancestors,
                        current,
                        order,
                        bound,
                    )?;

                    for indexed_filter in &mut indexed_filters[1..] {
                        match indexed_filter.search(
                            token,
                            storage,
                            event_ancestors,
                            current,
                            order,
                            current,
                        ) {
                            Some(found_entry) if found_entry != current => {
                                current = found_entry;
                                continue 'outer;
                            }
                            Some(_) => { /* continue */ }
                            None => {
                                match order {
                                    Order::Asc => current = current.saturating_add(1),
                                    Order::Desc => {
                                        current = Timestamp::new(current.get() - 1).unwrap()
                                    }
                                }
                                continue 'outer;
                            }
                        }
                    }

                    break Some(current);
                }
            }
            IndexedEventFilter::Or(indexed_filters) => {
                let mut next_entry =
                    indexed_filters[0].search(token, storage, event_ancestors, entry, order, bound);
                for indexed_filter in &mut indexed_filters[1..] {
                    let bound = next_entry.unwrap_or(bound);
                    if let Some(found_entry) =
                        indexed_filter.search(token, storage, event_ancestors, entry, order, bound)
                    {
                        if let Some(next_entry) = &mut next_entry {
                            match order {
                                Order::Asc if *next_entry > found_entry => {
                                    *next_entry = found_entry;
                                }
                                Order::Desc if *next_entry < found_entry => {
                                    *next_entry = found_entry;
                                }
                                _ => { /* continue */ }
                            }
                        } else {
                            next_entry = Some(found_entry);
                        }
                    }
                }

                next_entry
            }
        }
    }

    // This gives an estimate of the number of elements the filter may select.
    // It doesn't use any heuristics but rather returns the theoretical maximum.
    fn estimate_count(&self) -> usize {
        match self {
            IndexedEventFilter::Single(index, _) => {
                // we don't look at the basic filter because we can't really
                // guess how many elements it will select
                index.len()
            }
            IndexedEventFilter::And(filters) => {
                // since an element must pass all filters, we can only select
                // the minimum from a single filter
                filters.iter().map(Self::estimate_count).min().unwrap_or(0)
            }
            IndexedEventFilter::Or(filters) => {
                // since OR filters can be completely disjoint, we can possibly
                // yield the sum of all filters
                filters.iter().map(Self::estimate_count).sum()
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            IndexedEventFilter::Single(index, Some(_)) => {
                // The non-indexed filter may filter-out all elements or none of
                // them. So the full range is possible.
                (0, Some(index.len()))
            }
            IndexedEventFilter::Single(index, None) => {
                // Without a non-indexed filter, this will always yield the
                // number of elements it contains.
                (index.len(), Some(index.len()))
            }
            IndexedEventFilter::And(filters) => match filters.len() {
                0 => (0, Some(0)),
                1 => filters[0].size_hint(),
                _ => {
                    // With multiple filters AND-ed together, the potential min
                    // is zero (where none agree) and potential max is the
                    // smallest maximum.
                    let max = filters.iter().fold(None, |max, filter| {
                        merge(max, filter.size_hint().1, usize::min)
                    });

                    (0, max)
                }
            },
            IndexedEventFilter::Or(filters) => match filters.len() {
                0 => (0, Some(0)),
                1 => filters[0].size_hint(),
                _ => {
                    // With multiple filters OR-ed together, the potential min
                    // is the largest minimum and potential max is the sum of
                    // maximums.
                    filters.iter().fold((0, None), |(a_min, a_max), filter| {
                        let (min, max) = filter.size_hint();
                        (usize::max(a_min, min), merge(a_max, max, Add::add))
                    })
                }
            },
        }
    }

    pub fn trim_to_timeframe(&mut self, start: Timestamp, end: Timestamp) {
        match self {
            IndexedEventFilter::Single(index, _) => {
                let start_idx = index.lower_bound(&start);
                let end_idx = index.upper_bound(&end);

                *index = &index[start_idx..end_idx];
            }
            IndexedEventFilter::And(filters) => filters
                .iter_mut()
                .for_each(|f| f.trim_to_timeframe(start, end)),
            IndexedEventFilter::Or(filters) => filters
                .iter_mut()
                .for_each(|f| f.trim_to_timeframe(start, end)),
        }
    }

    pub fn optimize(&mut self) {
        match self {
            IndexedEventFilter::Single(_, _) => { /* nothing to do */ }
            IndexedEventFilter::And(filters) => filters.sort_by_key(Self::estimate_count),
            IndexedEventFilter::Or(filters) => filters.sort_by_key(Self::estimate_count),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum InputError {
    InvalidLevelValue,
    InvalidLevelOperator,
    InvalidNameOperator,
    InvalidInstanceValue,
    InvalidInstanceOperator,
    InvalidAttributeOperator,
    InvalidInherentProperty,
    InvalidDurationValue,
    MissingDurationOperator,
    InvalidDurationOperator,
    InvalidCreatedValue,
    MissingCreatedOperator,
    InvalidCreatedOperator,
    InvalidParentValue,
    InvalidParentOperator,
    InvalidStackValue,
    InvalidStackOperator,
}

#[derive(Debug, PartialEq, Deserialize)]
pub enum BasicEventFilter {
    Level(Level),
    Instance(InstanceKey),
    Ancestor(SpanKey),
    Attribute(String, String),
    And(Vec<BasicEventFilter>),
    Or(Vec<BasicEventFilter>),
}

impl BasicEventFilter {
    pub fn simplify(&mut self) {
        match self {
            BasicEventFilter::Level(_) => {}
            BasicEventFilter::Instance(_) => {}
            BasicEventFilter::Ancestor(_) => {}
            BasicEventFilter::Attribute(_, _) => {}
            BasicEventFilter::And(filters) => {
                for filter in &mut *filters {
                    filter.simplify()
                }

                if filters.len() == 1 {
                    let mut filters = std::mem::take(filters);
                    let filter = filters.pop().unwrap();
                    *self = filter;
                }
            }
            BasicEventFilter::Or(filters) => {
                for filter in &mut *filters {
                    filter.simplify()
                }

                if filters.len() == 1 {
                    let mut filters = std::mem::take(filters);
                    let filter = filters.pop().unwrap();
                    *self = filter;
                }
            }
        }
    }

    pub fn validate(predicate: FilterPredicate) -> Result<FilterPredicate, InputError> {
        use FilterPropertyKind::*;
        use FilterValueOperator::*;

        let property_kind = predicate
            .property_kind
            .unwrap_or(match predicate.property.as_str() {
                "level" | "instance" | "stack" => Inherent,
                _ => Attribute,
            });

        match (property_kind, predicate.property.as_str()) {
            (Inherent, "level") => {
                let _level = match predicate.value.as_str() {
                    "TRACE" => Level::Trace,
                    "DEBUG" => Level::Debug,
                    "INFO" => Level::Info,
                    "WARN" => Level::Warn,
                    "ERROR" => Level::Error,
                    _ => return Err(InputError::InvalidLevelValue),
                };

                let _above = match predicate.value_operator {
                    Some(Gte) => true,
                    None => false,
                    _ => return Err(InputError::InvalidLevelOperator),
                };
            }
            (Inherent, "instance") => {
                let _: InstanceId = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidInstanceValue)?;

                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidInstanceOperator);
                }
            }
            (Inherent, "stack") => {
                let _ =
                    parse_full_span_id(&predicate.value).ok_or(InputError::InvalidStackValue)?;

                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidStackOperator);
                }
            }
            (Inherent, _) => {
                return Err(InputError::InvalidInherentProperty);
            }
            (Attribute, _) => {
                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidAttributeOperator);
                }
            }
        };

        Ok(FilterPredicate {
            property_kind: Some(property_kind),
            ..predicate
        })
    }

    pub fn from_predicate(
        predicate: FilterPredicate,
        instance_key_map: &HashMap<InstanceId, InstanceKey>,
        span_key_map: &HashMap<(InstanceKey, SpanId), SpanKey>,
    ) -> Result<BasicEventFilter, InputError> {
        use FilterPropertyKind::*;
        use FilterValueOperator::*;

        let property_kind = predicate
            .property_kind
            .unwrap_or(match predicate.property.as_str() {
                "level" | "instance" | "stack" => Inherent,
                _ => Attribute,
            });

        let filter = match (property_kind, predicate.property.as_str()) {
            (Inherent, "level") => {
                let level = match predicate.value.as_str() {
                    "TRACE" => Level::Trace,
                    "DEBUG" => Level::Debug,
                    "INFO" => Level::Info,
                    "WARN" => Level::Warn,
                    "ERROR" => Level::Error,
                    _ => return Err(InputError::InvalidLevelValue),
                };

                let above = match predicate.value_operator {
                    Some(Gte) => true,
                    None => false,
                    _ => return Err(InputError::InvalidLevelOperator),
                };

                if above {
                    BasicEventFilter::Or(
                        ((level as i32)..5)
                            .map(|l| BasicEventFilter::Level(l.try_into().unwrap()))
                            .collect(),
                    )
                } else {
                    BasicEventFilter::Level(level)
                }
            }
            (Inherent, "instance") => {
                let instance_id: InstanceId = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidInstanceValue)?;

                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidInstanceOperator);
                }

                let instance_key = instance_key_map
                    .get(&instance_id)
                    .copied()
                    .unwrap_or(InstanceKey::MIN);

                BasicEventFilter::Instance(instance_key)
            }
            (Inherent, "stack") => {
                let (instance_id, span_id) =
                    parse_full_span_id(&predicate.value).ok_or(InputError::InvalidStackValue)?;

                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidStackOperator);
                }

                let instance_key = instance_key_map
                    .get(&instance_id)
                    .copied()
                    .unwrap_or(InstanceKey::MIN);
                let span_key = span_key_map
                    .get(&(instance_key, span_id))
                    .copied()
                    .unwrap_or(SpanKey::MIN);

                BasicEventFilter::Ancestor(span_key)
            }
            (Inherent, _) => {
                return Err(InputError::InvalidInherentProperty);
            }
            (Attribute, name) => {
                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidAttributeOperator);
                }

                BasicEventFilter::Attribute(name.to_owned(), predicate.value)
            }
        };

        Ok(filter)
    }

    pub(crate) fn matches<'b>(
        &self,
        token: &GhostToken<'b>,
        event_ancestors: &HashMap<Timestamp, Ancestors<'b>>,
        event: &Event,
    ) -> bool {
        match self {
            BasicEventFilter::Level(level) => event.level == *level,
            BasicEventFilter::Instance(instance_key) => event.instance_key == *instance_key,
            BasicEventFilter::Ancestor(span_key) => {
                event_ancestors[&event.key()].has_parent(*span_key)
            }
            BasicEventFilter::Attribute(attribute, value) => event_ancestors[&event.key()]
                .get_value(attribute, token)
                .map(|v| v == value)
                .unwrap_or(false),
            BasicEventFilter::And(filters) => filters
                .iter()
                .all(|f| f.matches(token, event_ancestors, event)),
            BasicEventFilter::Or(filters) => filters
                .iter()
                .any(|f| f.matches(token, event_ancestors, event)),
        }
    }
}

#[derive(Deserialize)]
pub enum NonIndexedEventFilter {
    Attribute(String, String),
}

impl NonIndexedEventFilter {
    fn matches<'b, S: Storage>(
        &self,
        token: &GhostToken<'b>,
        storage: &S,
        event_ancestors: &HashMap<Timestamp, Ancestors<'b>>,
        entry: Timestamp,
    ) -> bool {
        let log = storage.get_event(entry).unwrap();
        match self {
            NonIndexedEventFilter::Attribute(attribute, value) => event_ancestors[&log.timestamp]
                .get_value(attribute, token)
                .map(|v| v == value)
                .unwrap_or(false),
        }
    }
}

pub struct IndexedEventFilterIterator<'i, 'b, S> {
    filter: IndexedEventFilter<'i>,
    order: Order,
    start_key: Timestamp,
    end_key: Timestamp,
    storage: &'i S,
    token: &'i GhostToken<'b>,
    ancestors: &'i HashMap<Timestamp, Ancestors<'b>>,
}

impl<'i, 'b, S> IndexedEventFilterIterator<'i, 'b, S> {
    pub fn new(
        query: EventQuery,
        engine: &'i RawEngine<'b, S>,
    ) -> IndexedEventFilterIterator<'i, 'b, S> {
        let mut filter = BasicEventFilter::And(
            query
                .filter
                .into_iter()
                .map(|p| {
                    BasicEventFilter::from_predicate(
                        p,
                        &engine.instance_key_map,
                        &engine.span_key_map,
                    )
                    .unwrap()
                })
                .collect(),
        );
        filter.simplify();

        let mut filter = IndexedEventFilter::build(Some(filter), &engine.event_indexes);

        let mut start = query.start;
        let mut end = query.end;

        if let Some(prev) = query.previous {
            match query.order {
                Order::Asc => start = prev.saturating_add(1),
                Order::Desc => end = Timestamp::new(prev.get() - 1).unwrap(),
            }
        }

        filter.trim_to_timeframe(start, end);
        filter.optimize();

        let (start_key, end_key) = match query.order {
            Order::Asc => (start, end),
            Order::Desc => (end, start),
        };

        IndexedEventFilterIterator {
            filter,
            order: query.order,
            start_key,
            end_key,
            storage: &engine.storage,
            token: &engine.token,
            ancestors: &engine.event_ancestors,
        }
    }
}

impl<S> Iterator for IndexedEventFilterIterator<'_, '_, S>
where
    S: Storage,
{
    type Item = EventKey;

    fn next(&mut self) -> Option<EventKey> {
        let event_key = self.filter.search(
            self.token,
            self.storage,
            self.ancestors,
            self.start_key,
            self.order,
            self.end_key,
        )?;

        match self.order {
            Order::Asc => self.start_key = event_key.saturating_add(1),
            Order::Desc => self.start_key = Timestamp::new(event_key.get() - 1).unwrap(),
        };

        Some(event_key)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.filter.size_hint()
    }
}

#[derive(Deserialize)]
pub struct SpanQuery {
    pub filter: Vec<FilterPredicate>,
    pub order: Order,
    pub limit: usize,
    pub start: Timestamp,
    pub end: Timestamp,
    pub previous: Option<Timestamp>,
}

#[derive(Debug)]
pub enum IndexedSpanFilter<'i> {
    Single(&'i [Timestamp], Option<NonIndexedSpanFilter>),
    Stratified(&'i [Timestamp], Range<u64>, Option<NonIndexedSpanFilter>),
    And(Vec<IndexedSpanFilter<'i>>),
    Or(Vec<IndexedSpanFilter<'i>>),
}

impl IndexedSpanFilter<'_> {
    pub fn build(
        filter: Option<BasicSpanFilter>,
        span_indexes: &SpanIndexes,
    ) -> IndexedSpanFilter<'_> {
        let Some(filter) = filter else {
            return IndexedSpanFilter::Single(&span_indexes.all, None);
        };

        match filter {
            BasicSpanFilter::Level(level) => {
                IndexedSpanFilter::Single(&span_indexes.levels[level as usize], None)
            }
            BasicSpanFilter::Duration(duration_filter) => {
                let filters = span_indexes.durations.to_stratified_indexes();
                let filters = filters
                    .into_iter()
                    .filter_map(|(index, range)| {
                        match duration_filter.matches_duration_range(&range) {
                            Some(true) => Some(IndexedSpanFilter::Stratified(index, range, None)),
                            None => Some(IndexedSpanFilter::Stratified(
                                index,
                                range,
                                Some(NonIndexedSpanFilter::Duration(duration_filter.clone())),
                            )),
                            Some(false) => None,
                        }
                    })
                    .collect();

                IndexedSpanFilter::Or(filters)
            }
            BasicSpanFilter::Created(comparison) => {
                match comparison {
                    TimestampComparisonFilter::Gt(value) => {
                        let idx = span_indexes.all.upper_bound(&value);
                        IndexedSpanFilter::Single(&span_indexes.all[idx..], None)
                    }
                    TimestampComparisonFilter::Gte(value) => {
                        let idx = span_indexes.all.lower_bound(&value);
                        IndexedSpanFilter::Single(&span_indexes.all[idx..], None)
                    }
                    // TimestampComparisonFilter::Eq(value) => IndexedSpanFilter::Single(todo!(), None),
                    // TimestampComparisonFilter::Ne(value) => IndexedSpanFilter::Single(todo!(), None),
                    TimestampComparisonFilter::Lte(value) => {
                        let idx = span_indexes.all.upper_bound(&value);
                        IndexedSpanFilter::Single(&span_indexes.all[..idx], None)
                    }
                    TimestampComparisonFilter::Lt(value) => {
                        let idx = span_indexes.all.lower_bound(&value);
                        IndexedSpanFilter::Single(&span_indexes.all[..idx], None)
                    }
                }
            }
            BasicSpanFilter::Instance(instance_key) => {
                let instance_index = span_indexes
                    .instances
                    .get(&instance_key)
                    .map(Vec::as_slice)
                    .unwrap_or_default();

                IndexedSpanFilter::Single(instance_index, None)
            }
            BasicSpanFilter::Name(value) => {
                let name_index = span_indexes
                    .names
                    .get(&value)
                    .map(Vec::as_slice)
                    .unwrap_or_default();

                IndexedSpanFilter::Single(name_index, None)
            }
            BasicSpanFilter::Ancestor(ancestor_key) => {
                let index = &span_indexes.descendents[&ancestor_key];

                IndexedSpanFilter::Single(index, None)
            }
            BasicSpanFilter::Root => IndexedSpanFilter::Single(&span_indexes.roots, None),
            BasicSpanFilter::Attribute(attribute, value) => {
                if let Some(attr_index) = span_indexes.attributes.get(&attribute) {
                    let value_index = attr_index
                        .get(&value)
                        .map(Vec::as_slice)
                        .unwrap_or_default();

                    IndexedSpanFilter::Single(value_index, None)
                } else {
                    IndexedSpanFilter::Single(
                        &span_indexes.all,
                        Some(NonIndexedSpanFilter::Attribute(attribute, value)),
                    )
                }
            }
            BasicSpanFilter::And(filters) => IndexedSpanFilter::And(
                filters
                    .into_iter()
                    .map(|f| IndexedSpanFilter::build(Some(f), span_indexes))
                    .collect(),
            ),
            BasicSpanFilter::Or(filters) => IndexedSpanFilter::Or(
                filters
                    .into_iter()
                    .map(|f| IndexedSpanFilter::build(Some(f), span_indexes))
                    .collect(),
            ),
        }
    }

    // This basically checks if the filter can be trimmed by timeframe. Only
    // Stratified filters can be trimmed, so this checks if those are already
    // considered in the filter or not.
    fn is_stratified(&self) -> bool {
        match self {
            IndexedSpanFilter::Single(_, _) => false,
            IndexedSpanFilter::Stratified(_, _, _) => true,
            IndexedSpanFilter::And(filters) => filters.iter().any(|f| f.is_stratified()),
            IndexedSpanFilter::Or(filters) => filters.iter().all(|f| f.is_stratified()),
        }
    }

    // This searches for an entry equal to or beyond the provided entry
    #[allow(clippy::too_many_arguments)]
    pub fn search<'b, S: Storage>(
        &mut self,
        token: &GhostToken<'b>,
        storage: &S,
        span_ancestors: &HashMap<Timestamp, Ancestors<'b>>,
        mut entry: Timestamp, // this is the current lower bound for span keys
        order: Order,
        bound: Timestamp, // this is the current upper bound for span keys
        start: Timestamp, // this is the original search start time
                          // end: Timestamp,   // this is the original search end time
    ) -> Option<Timestamp> {
        match self {
            IndexedSpanFilter::Single(entries, filter) => match order {
                Order::Asc => loop {
                    let idx = entries.lower_bound(&entry);
                    *entries = &entries[idx..];
                    let found_entry = entries.first().cloned();

                    let found_entry = found_entry?;
                    if found_entry > bound {
                        return None;
                    }

                    if found_entry < start {
                        let span = storage.get_span(found_entry).unwrap();
                        if let Some(closed_at) = span.closed_at {
                            if closed_at <= start {
                                entry = found_entry.saturating_add(1);
                                continue;
                            }
                        }
                    }

                    if let Some(filter) = filter {
                        if filter.matches(token, storage, span_ancestors, found_entry) {
                            return Some(found_entry);
                        } else {
                            entry = found_entry.saturating_add(1);
                        }
                    } else {
                        return Some(found_entry);
                    }
                },
                Order::Desc => loop {
                    let idx = entries.upper_bound(&entry);
                    *entries = &entries[..idx];
                    let found_entry = entries.last().cloned();

                    let found_entry = found_entry?;
                    if found_entry < bound {
                        return None;
                    }

                    if found_entry < start {
                        let span = storage.get_span(found_entry).unwrap();
                        if let Some(closed_at) = span.closed_at {
                            if closed_at <= start {
                                entry = Timestamp::new(found_entry.get() - 1).unwrap();
                                continue;
                            }
                        }
                    }

                    if let Some(filter) = filter {
                        if filter.matches(token, storage, span_ancestors, found_entry) {
                            return Some(found_entry);
                        } else {
                            entry = Timestamp::new(found_entry.get() - 1).unwrap();
                        }
                    } else {
                        return Some(found_entry);
                    }
                },
            },
            IndexedSpanFilter::Stratified(entries, _, filter) => match order {
                Order::Asc => loop {
                    let idx = entries.lower_bound(&entry);
                    *entries = &entries[idx..];
                    let found_entry = entries.first().cloned();

                    let found_entry = found_entry?;
                    if found_entry > bound {
                        return None;
                    }

                    if found_entry < start {
                        let span = storage.get_span(found_entry).unwrap();
                        if let Some(closed_at) = span.closed_at {
                            if closed_at <= start {
                                entry = found_entry.saturating_add(1);
                                continue;
                            }
                        }
                    }

                    if let Some(filter) = filter {
                        if filter.matches(token, storage, span_ancestors, found_entry) {
                            return Some(found_entry);
                        } else {
                            entry = found_entry.saturating_add(1);
                        }
                    } else {
                        return Some(found_entry);
                    }
                },
                Order::Desc => loop {
                    let idx = entries.upper_bound(&entry);
                    *entries = &entries[..idx];
                    let found_entry = entries.last().cloned();

                    let found_entry = found_entry?;
                    if found_entry < bound {
                        return None;
                    }

                    if found_entry < start {
                        let span = storage.get_span(found_entry).unwrap();
                        if let Some(closed_at) = span.closed_at {
                            if closed_at <= start {
                                entry = Timestamp::new(found_entry.get() - 1).unwrap();
                                continue;
                            }
                        }
                    }

                    if let Some(filter) = filter {
                        if filter.matches(token, storage, span_ancestors, found_entry) {
                            return Some(found_entry);
                        } else {
                            entry = Timestamp::new(found_entry.get() - 1).unwrap();
                        }
                    } else {
                        return Some(found_entry);
                    }
                },
            },
            IndexedSpanFilter::And(indexed_filters) => {
                let mut current = entry;
                'outer: loop {
                    current = indexed_filters[0].search(
                        token,
                        storage,
                        span_ancestors,
                        current,
                        order,
                        bound,
                        start,
                    )?;
                    for indexed_filter in &mut indexed_filters[1..] {
                        match indexed_filter.search(
                            token,
                            storage,
                            span_ancestors,
                            current,
                            order,
                            current,
                            start,
                        ) {
                            Some(found_entry) if found_entry != current => {
                                current = found_entry;
                                continue 'outer;
                            }
                            Some(_) => { /* continue */ }
                            None => {
                                match order {
                                    Order::Asc => current = current.saturating_add(1),
                                    Order::Desc => {
                                        current = Timestamp::new(current.get() - 1).unwrap()
                                    }
                                }
                                continue 'outer;
                            }
                        }
                    }

                    break Some(current);
                }
            }
            IndexedSpanFilter::Or(indexed_filters) => {
                let mut next_entry = indexed_filters[0].search(
                    token,
                    storage,
                    span_ancestors,
                    entry,
                    order,
                    bound,
                    start,
                );
                for indexed_filter in &mut indexed_filters[1..] {
                    let bound = next_entry.unwrap_or(bound);
                    if let Some(found_entry) = indexed_filter.search(
                        token,
                        storage,
                        span_ancestors,
                        entry,
                        order,
                        bound,
                        start,
                    ) {
                        if let Some(next_entry) = &mut next_entry {
                            match order {
                                Order::Asc if *next_entry > found_entry => {
                                    *next_entry = found_entry;
                                }
                                Order::Desc if *next_entry < found_entry => {
                                    *next_entry = found_entry;
                                }
                                _ => { /* continue */ }
                            }
                        } else {
                            next_entry = Some(found_entry);
                        }
                    }
                }

                next_entry
            }
        }
    }

    // This gives an estimate of the number of elements the filter may select.
    // It doesn't use any heuristics but rather returns the theoretical maximum.
    fn estimate_count(&self) -> usize {
        match self {
            IndexedSpanFilter::Single(index, _) => {
                // we don't look at the basic filter because we can't really
                // guess how many elements it will select
                index.len()
            }
            IndexedSpanFilter::Stratified(index, _, _) => {
                // we don't look at the range since we can't really guess how
                // many elements it will select
                index.len()
            }
            IndexedSpanFilter::And(filters) => {
                // since an element must pass all filters, we can only select
                // the minimum from a single filter
                filters.iter().map(Self::estimate_count).min().unwrap_or(0)
            }
            IndexedSpanFilter::Or(filters) => {
                // since OR filters can be completely disjoint, we can possibly
                // yield the sum of all filters
                filters.iter().map(Self::estimate_count).sum()
            }
        }
    }

    pub fn optimize(&mut self) {
        match self {
            IndexedSpanFilter::Single(_, _) => { /* nothing to do */ }
            IndexedSpanFilter::Stratified(_, _, _) => { /* TODO: convert to AND and sort */ }
            IndexedSpanFilter::And(filters) => filters.sort_by_key(Self::estimate_count),
            IndexedSpanFilter::Or(filters) => filters.sort_by_key(Self::estimate_count),
        }
    }

    pub fn trim_to_timeframe(&mut self, start: Timestamp, end: Timestamp) {
        match self {
            IndexedSpanFilter::Single(index, _) => {
                // we can trim the end
                let trim_end = end;

                let end_idx = index.upper_bound(&trim_end);

                *index = &index[..end_idx];
            }
            IndexedSpanFilter::Stratified(index, duration_range, _) => {
                // we can trim to "max duration" before `start`
                let trim_start = Timestamp::new(start.get().saturating_sub(duration_range.end))
                    .unwrap_or(Timestamp::MIN);

                // we can trim by the end
                let trim_end = end;

                let start_idx = index.lower_bound(&trim_start);
                let end_idx = index.upper_bound(&trim_end);

                *index = &index[start_idx..end_idx];
            }
            IndexedSpanFilter::And(filters) => filters
                .iter_mut()
                .for_each(|f| f.trim_to_timeframe(start, end)),
            IndexedSpanFilter::Or(filters) => filters
                .iter_mut()
                .for_each(|f| f.trim_to_timeframe(start, end)),
        }
    }
}

impl<'a> IndexedSpanFilter<'a> {
    // This basically ensures that the filter can be trimmed by timeframe. Only
    // `Stratified` filters can be trimmed. If there are no stratified filters
    // or the filter is constructed in a way that not all filters are covered,
    // this will add the necessary `Stratified` filters to the root.
    pub fn ensure_stratified(&mut self, duration_index: &'a SpanDurationIndex) {
        if self.is_stratified() {
            return;
        }

        if let IndexedSpanFilter::And(filters) = self {
            let dfilters = duration_index.to_stratified_indexes();
            let dfilters = dfilters
                .into_iter()
                .map(|(index, range)| IndexedSpanFilter::Stratified(index, range, None))
                .collect();
            let dfilter = IndexedSpanFilter::Or(dfilters);

            filters.push(dfilter);
        } else {
            let this = std::mem::replace(self, IndexedSpanFilter::Single(&[], None));

            let dfilters = duration_index.to_stratified_indexes();
            let dfilters = dfilters
                .into_iter()
                .map(|(index, range)| IndexedSpanFilter::Stratified(index, range, None))
                .collect();
            let dfilter = IndexedSpanFilter::Or(dfilters);

            *self = IndexedSpanFilter::And(vec![this, dfilter])
        }
    }
}

#[derive(Debug, PartialEq, Deserialize)]
pub enum TimestampComparisonFilter {
    Gt(Timestamp),
    Gte(Timestamp),
    // Eq(Timestamp),
    // Ne(Timestamp),
    Lte(Timestamp),
    Lt(Timestamp),
}

#[derive(Debug, PartialEq, Deserialize)]
pub enum BasicSpanFilter {
    Level(Level),
    Duration(DurationFilter),
    Created(TimestampComparisonFilter),
    Instance(InstanceKey),
    Name(String),
    Ancestor(SpanKey),
    Root,
    Attribute(String, String),
    And(Vec<BasicSpanFilter>),
    Or(Vec<BasicSpanFilter>),
}

impl BasicSpanFilter {
    fn simplify(&mut self) {
        match self {
            BasicSpanFilter::Level(_) => {}
            BasicSpanFilter::Duration(_) => {}
            BasicSpanFilter::Created(_) => {}
            BasicSpanFilter::Instance(_) => {}
            BasicSpanFilter::Name(_) => {}
            BasicSpanFilter::Ancestor(_) => {}
            BasicSpanFilter::Root => {}
            BasicSpanFilter::Attribute(_, _) => {}
            BasicSpanFilter::And(filters) => {
                for filter in &mut *filters {
                    filter.simplify()
                }

                if filters.len() == 1 {
                    let mut filters = std::mem::take(filters);
                    let filter = filters.pop().unwrap();
                    *self = filter;
                }
            }
            BasicSpanFilter::Or(filters) => {
                for filter in &mut *filters {
                    filter.simplify()
                }

                if filters.len() == 1 {
                    let mut filters = std::mem::take(filters);
                    let filter = filters.pop().unwrap();
                    *self = filter;
                }
            }
        }
    }

    pub fn validate(predicate: FilterPredicate) -> Result<FilterPredicate, InputError> {
        use FilterPropertyKind::*;
        use FilterValueOperator::*;

        let property_kind = predicate
            .property_kind
            .unwrap_or(match predicate.property.as_str() {
                "level" | "instance" | "duration" | "name" | "parent" | "created" | "stack" => {
                    Inherent
                }
                _ => Attribute,
            });

        match (property_kind, predicate.property.as_str()) {
            (Inherent, "level") => {
                let _level = match predicate.value.as_str() {
                    "TRACE" => Level::Trace,
                    "DEBUG" => Level::Debug,
                    "INFO" => Level::Info,
                    "WARN" => Level::Warn,
                    "ERROR" => Level::Error,
                    _ => return Err(InputError::InvalidLevelValue),
                };

                let _above = match predicate.value_operator {
                    Some(Gte) => true,
                    None => false,
                    _ => return Err(InputError::InvalidLevelOperator),
                };
            }
            (Inherent, "duration") => {
                let _: u64 = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidDurationValue)?;

                match predicate.value_operator {
                    Some(Gt) => {}
                    Some(Lt) => {}
                    None => return Err(InputError::MissingDurationOperator),
                    _ => return Err(InputError::InvalidDurationOperator),
                }
            }
            (Inherent, "name") => {
                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidNameOperator);
                }
            }
            (Inherent, "instance") => {
                let _: InstanceId = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidInstanceValue)?;
            }
            (Inherent, "created") => {
                let _: Timestamp = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidCreatedValue)?;

                match predicate.value_operator {
                    Some(Gt | Gte) => {}
                    Some(Lt | Lte) => {}
                    None => return Err(InputError::MissingCreatedOperator),
                    _ => return Err(InputError::InvalidCreatedOperator),
                }
            }
            (Inherent, "parent") => {
                if predicate.value != "none" {
                    return Err(InputError::InvalidParentValue);
                }

                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidParentOperator);
                }
            }
            (Inherent, "stack") => {
                let _ =
                    parse_full_span_id(&predicate.value).ok_or(InputError::InvalidStackValue)?;

                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidStackOperator);
                }
            }
            (Inherent, _) => {
                return Err(InputError::InvalidInherentProperty);
            }
            (Attribute, _) => {
                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidAttributeOperator);
                }
            }
        }

        Ok(FilterPredicate {
            property_kind: Some(property_kind),
            ..predicate
        })
    }

    pub fn from_predicate(
        predicate: FilterPredicate,
        instance_key_map: &HashMap<InstanceId, InstanceKey>,
        span_key_map: &HashMap<(InstanceKey, SpanId), SpanKey>,
    ) -> Result<BasicSpanFilter, InputError> {
        use FilterPropertyKind::*;
        use FilterValueOperator::*;

        let property_kind = predicate
            .property_kind
            .unwrap_or(match predicate.property.as_str() {
                "level" | "instance" | "duration" | "name" | "parent" | "created" | "stack" => {
                    Inherent
                }
                _ => Attribute,
            });

        let filter = match (property_kind, predicate.property.as_str()) {
            (Inherent, "level") => {
                let level = match predicate.value.as_str() {
                    "TRACE" => Level::Trace,
                    "DEBUG" => Level::Debug,
                    "INFO" => Level::Info,
                    "WARN" => Level::Warn,
                    "ERROR" => Level::Error,
                    _ => return Err(InputError::InvalidLevelValue),
                };

                let above = match predicate.value_operator {
                    Some(Gte) => true,
                    None => false,
                    _ => return Err(InputError::InvalidLevelOperator),
                };

                if above {
                    BasicSpanFilter::Or(
                        ((level as i32)..5)
                            .map(|l| BasicSpanFilter::Level(l.try_into().unwrap()))
                            .collect(),
                    )
                } else {
                    BasicSpanFilter::Level(level)
                }
            }
            (Inherent, "duration") => {
                let measure: u64 = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidDurationValue)?;

                let filter = match predicate.value_operator {
                    Some(Gt) => DurationFilter::Gt(measure),
                    Some(Lt) => DurationFilter::Lt(measure),
                    None => return Err(InputError::MissingDurationOperator),
                    _ => return Err(InputError::InvalidDurationOperator),
                };

                BasicSpanFilter::Duration(filter)
            }
            (Inherent, "name") => {
                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidNameOperator);
                }

                BasicSpanFilter::Name(predicate.value)
            }
            (Inherent, "instance") => {
                let instance_id: InstanceId = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidInstanceValue)?;

                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidInstanceOperator);
                }

                let instance_key = instance_key_map
                    .get(&instance_id)
                    .copied()
                    .unwrap_or(InstanceKey::MIN);

                BasicSpanFilter::Instance(instance_key)
            }
            (Inherent, "created") => {
                let at: Timestamp = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidCreatedValue)?;

                let filter = match predicate.value_operator {
                    Some(Gte) => TimestampComparisonFilter::Gte(at),
                    Some(Gt) => TimestampComparisonFilter::Gt(at),
                    Some(Lte) => TimestampComparisonFilter::Lte(at),
                    Some(Lt) => TimestampComparisonFilter::Lt(at),
                    None => return Err(InputError::MissingCreatedOperator),
                    _ => return Err(InputError::InvalidCreatedOperator),
                };

                BasicSpanFilter::Created(filter)
            }
            (Inherent, "parent") => {
                if predicate.value != "none" {
                    return Err(InputError::InvalidParentValue);
                }

                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidParentOperator);
                }

                BasicSpanFilter::Root
            }
            (Inherent, "stack") => {
                let (instance_id, span_id) =
                    parse_full_span_id(&predicate.value).ok_or(InputError::InvalidStackValue)?;

                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidStackOperator);
                }

                let instance_key = instance_key_map
                    .get(&instance_id)
                    .copied()
                    .unwrap_or(InstanceKey::MIN);
                let span_key = span_key_map
                    .get(&(instance_key, span_id))
                    .copied()
                    .unwrap_or(SpanKey::MIN);

                BasicSpanFilter::Ancestor(span_key)
            }
            (Inherent, _) => {
                return Err(InputError::InvalidInherentProperty);
            }
            (Attribute, name) => {
                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidAttributeOperator);
                }

                BasicSpanFilter::Attribute(name.to_owned(), predicate.value)
            }
        };

        Ok(filter)
    }
}

#[derive(Debug, Deserialize)]
pub enum NonIndexedSpanFilter {
    Duration(DurationFilter),
    Attribute(String, String),
}

impl NonIndexedSpanFilter {
    fn matches<'b, S: Storage>(
        &self,
        token: &GhostToken<'b>,
        storage: &S,
        span_ancestors: &HashMap<Timestamp, Ancestors<'b>>,
        entry: Timestamp,
    ) -> bool {
        let span = storage.get_span(entry).unwrap();
        match self {
            NonIndexedSpanFilter::Duration(filter) => span
                .duration()
                .map(|duration| match filter {
                    DurationFilter::Gt(measure) => duration > *measure,
                    DurationFilter::Lt(measure) => duration < *measure,
                })
                .unwrap_or(false),
            NonIndexedSpanFilter::Attribute(attribute, value) => span_ancestors[&span.created_at]
                .get_value(attribute, token)
                .map(|v| v == value)
                .unwrap_or(false),
        }
    }
}

#[derive(Debug, PartialEq, Clone, Deserialize)]
pub enum DurationFilter {
    Gt(u64),
    Lt(u64),
}

impl DurationFilter {
    pub fn matches_duration_range(&self, range: &Range<u64>) -> Option<bool> {
        match self {
            // --y--[ p ]--n--
            DurationFilter::Gt(measure) if *measure <= range.start => Some(true),
            DurationFilter::Gt(measure) if *measure > range.end => Some(false),
            DurationFilter::Gt(_) => None,
            // --n--[ p ]--y--
            DurationFilter::Lt(measure) if *measure >= range.end => Some(true),
            DurationFilter::Lt(measure) if *measure < range.start => Some(false),
            DurationFilter::Lt(_) => None,
        }
    }
}

pub struct IndexedSpanFilterIterator<'i, 'b, S> {
    filter: IndexedSpanFilter<'i>,
    order: Order,
    curr_key: Timestamp,
    start_key: Timestamp,
    end_key: Timestamp,
    storage: &'i S,
    token: &'i GhostToken<'b>,
    ancestors: &'i HashMap<Timestamp, Ancestors<'b>>,
}

impl<'i, 'b, S> IndexedSpanFilterIterator<'i, 'b, S> {
    pub fn new(
        query: SpanQuery,
        engine: &'i RawEngine<'b, S>,
    ) -> IndexedSpanFilterIterator<'i, 'b, S> {
        let mut filter = BasicSpanFilter::And(
            query
                .filter
                .into_iter()
                .map(|p| {
                    BasicSpanFilter::from_predicate(
                        p,
                        &engine.instance_key_map,
                        &engine.span_key_map,
                    )
                    .unwrap()
                })
                .collect(),
        );
        filter.simplify();

        let mut filter = IndexedSpanFilter::build(Some(filter), &engine.span_indexes);

        let curr;
        let mut start = query.start;
        let mut end = query.end;

        // if order is asc
        // - if previous & greater than or = start, then start = previous + 1, curr = start
        // - if previous & less than start, then start = start, curr = previous + 1
        // - if no previous, then start = start, curr = MIN
        // if order is desc
        // - if previous & greater than start, then end = previous - 1, curr = end
        // - if previous & less than or = start, then end = start, curr = previous - 1
        // - if no previous, then end = end, curr = end

        match (query.order, query.previous) {
            (Order::Asc, Some(prev)) if prev >= query.start => {
                start = prev.saturating_add(1);
                curr = start;
            }
            (Order::Asc, Some(prev)) => {
                curr = prev.saturating_add(1);
            }
            (Order::Asc, None) => {
                curr = Timestamp::MIN;
            }
            (Order::Desc, Some(prev)) if prev > query.start => {
                end = Timestamp::new(prev.get() - 1).unwrap();
                curr = end;
            }
            (Order::Desc, Some(prev)) => {
                end = start;
                curr = Timestamp::new(prev.get() - 1).unwrap();
            }
            (Order::Desc, None) => {
                curr = end;
            }
        }

        filter.ensure_stratified(&engine.span_indexes.durations);
        filter.trim_to_timeframe(start, end);
        filter.optimize();

        let (start_key, end_key) = match query.order {
            Order::Asc => (start, end),
            Order::Desc => (start, Timestamp::MIN),
        };

        IndexedSpanFilterIterator {
            filter,
            order: query.order,
            curr_key: curr,
            end_key,
            start_key,
            storage: &engine.storage,
            token: &engine.token,
            ancestors: &engine.span_ancestors,
        }
    }

    pub fn new_internal(
        filter: IndexedSpanFilter<'i>,
        engine: &'i RawEngine<'b, S>,
    ) -> IndexedSpanFilterIterator<'i, 'b, S> {
        IndexedSpanFilterIterator {
            filter,
            order: Order::Asc,
            curr_key: Timestamp::MIN,
            end_key: Timestamp::MAX,
            start_key: Timestamp::MIN,
            storage: &engine.storage,
            token: &engine.token,
            ancestors: &engine.span_ancestors,
        }
    }
}

impl<S> Iterator for IndexedSpanFilterIterator<'_, '_, S>
where
    S: Storage,
{
    type Item = SpanKey;

    fn next(&mut self) -> Option<SpanKey> {
        let span_key = self.filter.search(
            self.token,
            self.storage,
            self.ancestors,
            self.curr_key,
            self.order,
            self.end_key,
            self.start_key,
        )?;

        match self.order {
            Order::Asc => self.curr_key = span_key.saturating_add(1),
            Order::Desc => self.curr_key = Timestamp::new(span_key.get() - 1).unwrap(),
        };

        Some(span_key)
    }

    // fn size_hint(&self) -> (usize, Option<usize>) {
    //     self.filter.size_hint()
    // }
}

#[derive(Deserialize)]
pub struct InstanceQuery {
    pub filter: Vec<FilterPredicate>,
    pub order: Order,
    pub limit: usize,
    pub start: Timestamp,
    pub end: Timestamp,
    pub previous: Option<Timestamp>,
}

#[derive(Debug, PartialEq, Deserialize)]
pub enum BasicInstanceFilter {
    Duration(DurationFilter),
    Connected(TimestampComparisonFilter),
    Disconnected(TimestampComparisonFilter),
    Attribute(String, String),
    And(Vec<BasicInstanceFilter>),
    Or(Vec<BasicInstanceFilter>),
}

impl BasicInstanceFilter {
    pub fn simplify(&mut self) {
        match self {
            BasicInstanceFilter::Duration(_) => {}
            BasicInstanceFilter::Connected(_) => {}
            BasicInstanceFilter::Disconnected(_) => {}
            BasicInstanceFilter::Attribute(_, _) => {}
            BasicInstanceFilter::And(filters) => {
                for filter in &mut *filters {
                    filter.simplify()
                }

                if filters.len() == 1 {
                    let mut filters = std::mem::take(filters);
                    let filter = filters.pop().unwrap();
                    *self = filter;
                }
            }
            BasicInstanceFilter::Or(filters) => {
                for filter in &mut *filters {
                    filter.simplify()
                }

                if filters.len() == 1 {
                    let mut filters = std::mem::take(filters);
                    let filter = filters.pop().unwrap();
                    *self = filter;
                }
            }
        }
    }

    pub fn validate(predicate: FilterPredicate) -> Result<FilterPredicate, InputError> {
        use FilterPropertyKind::*;
        use FilterValueOperator::*;

        let property_kind = predicate
            .property_kind
            .unwrap_or(match predicate.property.as_str() {
                "duration" | "connected" | "disconnected" => Inherent,
                _ => Attribute,
            });

        match (property_kind, predicate.property.as_str()) {
            (Inherent, "duration") => {
                let _: u64 = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidDurationValue)?;

                match predicate.value_operator {
                    Some(Gt) => {}
                    Some(Lt) => {}
                    None => return Err(InputError::MissingDurationOperator),
                    _ => return Err(InputError::InvalidDurationOperator),
                }
            }
            (Inherent, "connected") => {
                let _: Timestamp = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidCreatedValue)?;

                match predicate.value_operator {
                    Some(Gt | Gte) => {}
                    Some(Lt | Lte) => {}
                    None => return Err(InputError::MissingCreatedOperator),
                    _ => return Err(InputError::InvalidCreatedOperator),
                }
            }
            (Inherent, "disconnected") => {
                let _: Timestamp = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidCreatedValue)?;

                match predicate.value_operator {
                    Some(Gt | Gte) => {}
                    Some(Lt | Lte) => {}
                    None => return Err(InputError::MissingCreatedOperator),
                    _ => return Err(InputError::InvalidCreatedOperator),
                }
            }
            (Inherent, _) => {
                return Err(InputError::InvalidInherentProperty);
            }
            (Attribute, _) => {
                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidAttributeOperator);
                }
            }
        }

        Ok(FilterPredicate {
            property_kind: Some(property_kind),
            ..predicate
        })
    }

    pub fn from_predicate(predicate: FilterPredicate) -> Result<BasicInstanceFilter, InputError> {
        use FilterPropertyKind::*;
        use FilterValueOperator::*;

        let property_kind = predicate
            .property_kind
            .unwrap_or(match predicate.property.as_str() {
                "duration" | "connected" | "disconnected" => Inherent,
                _ => Attribute,
            });

        let filter = match (property_kind, predicate.property.as_str()) {
            (Inherent, "duration") => {
                let measure: u64 = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidDurationValue)?;

                let filter = match predicate.value_operator {
                    Some(Gt) => DurationFilter::Gt(measure),
                    Some(Lt) => DurationFilter::Lt(measure),
                    None => return Err(InputError::MissingDurationOperator),
                    _ => return Err(InputError::InvalidDurationOperator),
                };

                BasicInstanceFilter::Duration(filter)
            }
            (Inherent, "connected") => {
                let at: Timestamp = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidCreatedValue)?;

                let filter = match predicate.value_operator {
                    Some(Gte) => TimestampComparisonFilter::Gte(at),
                    Some(Gt) => TimestampComparisonFilter::Gt(at),
                    Some(Lte) => TimestampComparisonFilter::Lte(at),
                    Some(Lt) => TimestampComparisonFilter::Lt(at),
                    None => return Err(InputError::MissingCreatedOperator),
                    _ => return Err(InputError::InvalidCreatedOperator),
                };

                BasicInstanceFilter::Connected(filter)
            }
            (Inherent, "disconnected") => {
                let at: Timestamp = predicate
                    .value
                    .parse()
                    .map_err(|_| InputError::InvalidCreatedValue)?;

                let filter = match predicate.value_operator {
                    Some(Gte) => TimestampComparisonFilter::Gte(at),
                    Some(Gt) => TimestampComparisonFilter::Gt(at),
                    Some(Lte) => TimestampComparisonFilter::Lte(at),
                    Some(Lt) => TimestampComparisonFilter::Lt(at),
                    None => return Err(InputError::MissingCreatedOperator),
                    _ => return Err(InputError::InvalidCreatedOperator),
                };

                BasicInstanceFilter::Disconnected(filter)
            }
            (Inherent, _) => {
                return Err(InputError::InvalidInherentProperty);
            }
            (Attribute, name) => {
                if predicate.value_operator.is_some() {
                    return Err(InputError::InvalidAttributeOperator);
                }

                BasicInstanceFilter::Attribute(name.to_owned(), predicate.value)
            }
        };

        Ok(filter)
    }

    pub fn matches<S: Storage>(&self, storage: &S, entry: Timestamp) -> bool {
        let instance = storage.get_instance(entry).unwrap();
        match self {
            BasicInstanceFilter::Duration(filter) => instance
                .duration()
                .map(|duration| match filter {
                    DurationFilter::Gt(measure) => duration > *measure,
                    DurationFilter::Lt(measure) => duration < *measure,
                })
                .unwrap_or(false),
            BasicInstanceFilter::Connected(filter) => match filter {
                TimestampComparisonFilter::Gt(timestamp) => instance.connected_at > *timestamp,
                TimestampComparisonFilter::Gte(timestamp) => instance.connected_at >= *timestamp,
                TimestampComparisonFilter::Lte(timestamp) => instance.connected_at <= *timestamp,
                TimestampComparisonFilter::Lt(timestamp) => instance.connected_at < *timestamp,
            },
            BasicInstanceFilter::Disconnected(filter) => {
                let Some(disconnected_at) = instance.disconnected_at else {
                    return false;
                };

                match filter {
                    TimestampComparisonFilter::Gt(timestamp) => disconnected_at > *timestamp,
                    TimestampComparisonFilter::Gte(timestamp) => disconnected_at >= *timestamp,
                    TimestampComparisonFilter::Lte(timestamp) => disconnected_at <= *timestamp,
                    TimestampComparisonFilter::Lt(timestamp) => disconnected_at < *timestamp,
                }
            }
            BasicInstanceFilter::Attribute(attribute, value) => instance
                .fields
                .get(attribute)
                .map(|v| v == value)
                .unwrap_or(false),
            BasicInstanceFilter::And(filters) => filters.iter().all(|f| f.matches(storage, entry)),
            BasicInstanceFilter::Or(filters) => filters.iter().any(|f| f.matches(storage, entry)),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    Asc,
    Desc,
}

#[allow(dead_code)]
pub trait BoundSearch<T> {
    // This finds the first index of an item that is not less than the provided
    // item. This works via a binary-search algorithm.
    //
    // NOTE: The result is only meaningful if the input is sorted.
    fn lower_bound(&self, item: &T) -> usize;

    // This finds the first index of an item that is greater than the provided
    // item. This works via a binary-search algorithm.
    //
    // NOTE: The result is only meaningful if the input is sorted.
    fn upper_bound(&self, item: &T) -> usize;

    // This finds the first index of an item that is not less than the provided
    // item. This works via a binary-expansion-search algorithm, i.e. it checks
    // indexes geometrically starting from the beginning and then uses binary
    // -search within those bounds. This method is good if the item is expected
    // near the beginning.
    //
    // NOTE: The result is only meaningful if the input is sorted.
    fn lower_bound_via_expansion(&self, item: &T) -> usize;

    // This finds the first index of an item that is greater than the provided
    // item. This works via a binary-expansion-search algorithm, i.e. it checks
    // indexes geometrically starting from the end and then uses binary-search
    // within those bounds. This method is good if the item is expected near the
    // end.
    //
    // NOTE: The result is only meaningful if the input is sorted.
    fn upper_bound_via_expansion(&self, item: &T) -> usize;
}

impl<T: Ord> BoundSearch<T> for [T] {
    fn lower_bound(&self, item: &T) -> usize {
        self.binary_search_by(|current_item| match current_item.cmp(item) {
            Ordering::Greater => Ordering::Greater,
            Ordering::Equal => Ordering::Greater,
            Ordering::Less => Ordering::Less,
        })
        .unwrap_or_else(|idx| idx)
    }

    fn upper_bound(&self, item: &T) -> usize {
        self.binary_search_by(|current_item| match current_item.cmp(item) {
            Ordering::Greater => Ordering::Greater,
            Ordering::Equal => Ordering::Less,
            Ordering::Less => Ordering::Less,
        })
        .unwrap_or_else(|idx| idx)
    }

    fn lower_bound_via_expansion(&self, item: &T) -> usize {
        let len = self.len();
        for (start, mut end) in std::iter::successors(Some((0, 1)), |&(_, j)| Some((j, j * 2))) {
            if end >= len {
                end = len
            } else if &self[end] < item {
                continue;
            }

            return self[start..end].lower_bound(item) + start;
        }

        unreachable!()
    }

    fn upper_bound_via_expansion(&self, item: &T) -> usize {
        let len = self.len();
        for (start, mut end) in std::iter::successors(Some((0, 1)), |&(_, j)| Some((j, j * 2))) {
            if end >= len {
                end = len
            } else if &self[len - end] > item {
                continue;
            }

            return self[len - end..len - start].upper_bound(item) + (len - end);
        }

        unreachable!()
    }
}

fn merge<T>(a: Option<T>, b: Option<T>, f: impl FnOnce(T, T) -> T) -> Option<T> {
    // I wish this was in the standard library

    match (a, b) {
        (Some(a), Some(b)) => Some(f(a, b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_on_empty_slice() {
        assert_eq!([].lower_bound(&0), 0);
        assert_eq!([].upper_bound(&0), 0);
        assert_eq!([].lower_bound_via_expansion(&0), 0);
        assert_eq!([].upper_bound_via_expansion(&0), 0);
    }

    #[test]
    fn bounds_on_single_slice() {
        assert_eq!([1].lower_bound(&0), 0);
        assert_eq!([1].upper_bound(&0), 0);
        assert_eq!([1].lower_bound_via_expansion(&0), 0);
        assert_eq!([1].upper_bound_via_expansion(&0), 0);

        assert_eq!([1].lower_bound(&1), 0);
        assert_eq!([1].upper_bound(&1), 1);
        assert_eq!([1].lower_bound_via_expansion(&1), 0);
        assert_eq!([1].upper_bound_via_expansion(&1), 1);

        assert_eq!([1].lower_bound(&2), 1);
        assert_eq!([1].upper_bound(&2), 1);
        assert_eq!([1].lower_bound_via_expansion(&2), 1);
        assert_eq!([1].upper_bound_via_expansion(&2), 1);
    }

    #[test]
    fn bounds_for_duplicate_item() {
        assert_eq!([0, 0, 1, 1, 2, 2].lower_bound(&-1), 0);
        assert_eq!([0, 0, 1, 1, 2, 2].upper_bound(&-1), 0);
        assert_eq!([0, 0, 1, 1, 2, 2].lower_bound_via_expansion(&-1), 0);
        assert_eq!([0, 0, 1, 1, 2, 2].upper_bound_via_expansion(&-1), 0);

        assert_eq!([0, 0, 1, 1, 2, 2].lower_bound(&0), 0);
        assert_eq!([0, 0, 1, 1, 2, 2].upper_bound(&0), 2);
        assert_eq!([0, 0, 1, 1, 2, 2].lower_bound_via_expansion(&0), 0);
        assert_eq!([0, 0, 1, 1, 2, 2].upper_bound_via_expansion(&0), 2);

        assert_eq!([0, 0, 1, 1, 2, 2].lower_bound(&1), 2);
        assert_eq!([0, 0, 1, 1, 2, 2].upper_bound(&1), 4);
        assert_eq!([0, 0, 1, 1, 2, 2].lower_bound_via_expansion(&1), 2);
        assert_eq!([0, 0, 1, 1, 2, 2].upper_bound_via_expansion(&1), 4);

        assert_eq!([0, 0, 1, 1, 2, 2].lower_bound(&2), 4);
        assert_eq!([0, 0, 1, 1, 2, 2].upper_bound(&2), 6);
        assert_eq!([0, 0, 1, 1, 2, 2].lower_bound_via_expansion(&2), 4);
        assert_eq!([0, 0, 1, 1, 2, 2].upper_bound_via_expansion(&2), 6);

        assert_eq!([0, 0, 1, 1, 2, 2].lower_bound(&3), 6);
        assert_eq!([0, 0, 1, 1, 2, 2].upper_bound(&3), 6);
        assert_eq!([0, 0, 1, 1, 2, 2].lower_bound_via_expansion(&3), 6);
        assert_eq!([0, 0, 1, 1, 2, 2].upper_bound_via_expansion(&3), 6);
    }

    #[test]
    fn bounds_for_missing_item() {
        assert_eq!([0, 0, 2, 2].lower_bound(&1), 2);
        assert_eq!([0, 0, 2, 2].upper_bound(&1), 2);
        assert_eq!([0, 0, 2, 2].lower_bound_via_expansion(&1), 2);
        assert_eq!([0, 0, 2, 2].upper_bound_via_expansion(&1), 2);
    }

    // #[test]
    // fn parse_level_into_filter() {
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:TRACE").unwrap(),
    //         BasicEventFilter::Level(0),
    //     );
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:DEBUG").unwrap(),
    //         BasicEventFilter::Level(1),
    //     );
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:INFO").unwrap(),
    //         BasicEventFilter::Level(2),
    //     );
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:WARN").unwrap(),
    //         BasicEventFilter::Level(3),
    //     );
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:ERROR").unwrap(),
    //         BasicEventFilter::Level(4),
    //     );
    // }

    // #[test]
    // fn parse_level_plus_into_filter() {
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:TRACE+").unwrap(),
    //         BasicEventFilter::Or(vec![
    //             BasicEventFilter::Level(0),
    //             BasicEventFilter::Level(1),
    //             BasicEventFilter::Level(2),
    //             BasicEventFilter::Level(3),
    //             BasicEventFilter::Level(4),
    //         ])
    //     );
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:DEBUG+").unwrap(),
    //         BasicEventFilter::Or(vec![
    //             BasicEventFilter::Level(1),
    //             BasicEventFilter::Level(2),
    //             BasicEventFilter::Level(3),
    //             BasicEventFilter::Level(4),
    //         ])
    //     );
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:INFO+").unwrap(),
    //         BasicEventFilter::Or(vec![
    //             BasicEventFilter::Level(2),
    //             BasicEventFilter::Level(3),
    //             BasicEventFilter::Level(4),
    //         ])
    //     );
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:WARN+").unwrap(),
    //         BasicEventFilter::Or(vec![BasicEventFilter::Level(3), BasicEventFilter::Level(4),])
    //     );
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:ERROR+").unwrap(),
    //         BasicEventFilter::Level(4)
    //     );
    // }

    // #[test]
    // fn parse_attribute_into_filter() {
    //     assert_eq!(
    //         BasicEventFilter::from_str("@attr1:A").unwrap(),
    //         BasicEventFilter::Attribute("attr1".into(), "A".into()),
    //     );
    // }

    // #[test]
    // fn parse_multiple_into_filter() {
    //     assert_eq!(
    //         BasicEventFilter::from_str("@attr1:A @attr2:B").unwrap(),
    //         BasicEventFilter::And(vec![
    //             BasicEventFilter::Attribute("attr1".into(), "A".into()),
    //             BasicEventFilter::Attribute("attr2".into(), "B".into()),
    //         ])
    //     );
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:ERROR @attr2:B").unwrap(),
    //         BasicEventFilter::And(vec![
    //             BasicEventFilter::Level(4),
    //             BasicEventFilter::Attribute("attr2".into(), "B".into()),
    //         ])
    //     );
    //     assert_eq!(
    //         BasicEventFilter::from_str("#level:INFO+ @attr2:B").unwrap(),
    //         BasicEventFilter::And(vec![
    //             BasicEventFilter::Or(vec![
    //                 BasicEventFilter::Level(2),
    //                 BasicEventFilter::Level(3),
    //                 BasicEventFilter::Level(4),
    //             ]),
    //             BasicEventFilter::Attribute("attr2".into(), "B".into()),
    //         ])
    //     );
    // }

    // #[test]
    // fn parse_duration_into_filter() {
    //     assert_eq!(
    //         BasicSpanFilter::from_str("#duration:>1000000").unwrap(),
    //         BasicSpanFilter::Duration(DurationFilter::Gt(1000000.try_into().unwrap()))
    //     );
    //     assert_eq!(
    //         BasicSpanFilter::from_str("#duration:<1000000").unwrap(),
    //         BasicSpanFilter::Duration(DurationFilter::Lt(1000000.try_into().unwrap()))
    //     );
    // }
}