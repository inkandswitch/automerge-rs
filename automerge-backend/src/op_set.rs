//! The OpSet is where most of the interesting work is done in this library.
//! It maintains a mapping from each object ID to a set of concurrent
//! operations which have been seen for that object ID.
//!
//! When the client requests the value of the CRDT (via
//! document::state) the implementation fetches the root object ID's history
//! and then recursively walks through the tree of histories constructing the
//! state. Obviously this is not very efficient.
use crate::actor_states::ActorStates;
use crate::concurrent_operations::ConcurrentOperations;
use crate::error::AutomergeError;
use crate::object_store::ObjectStore;
use crate::operation_with_metadata::OperationWithMetadata;
use crate::protocol::{Change, Clock, ElementID, ObjectID, Operation};
use crate::{ActorID, Diff, DiffAction};
use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::BuildHasher;

/// The OpSet manages an ObjectStore, and a queue of incoming changes in order
/// to ensure that operations are delivered to the object store in causal order
///
/// Whenever a new change is received we iterate through any causally ready
/// changes in the queue and apply them to the object store, then repeat until
/// there are no causally ready changes left. The end result of this is that
/// the object store will contain sets of concurrent operations for each object
/// ID or element ID.
///
/// When we want to get the state of the CRDT we walk through the
/// object store, starting with the root object ID and constructing the value
/// at each node by examining the concurrent operationsi which are active for
/// that node.
#[derive(Debug, PartialEq, Clone)]
pub struct OpSet {
    pub object_store: ObjectStore,
    queue: Vec<Change>,
    pub clock: Clock,
    undo_pos: usize,
    pub undo_stack: Vec<Vec<Operation>>,
    pub redo_stack: Vec<Vec<Operation>>,
    pub states: ActorStates,
}

impl OpSet {
    pub fn init() -> OpSet {
        OpSet {
            object_store: ObjectStore::new(),
            queue: Vec::new(),
            clock: Clock::empty(),
            undo_pos: 0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            states: ActorStates::new(),
        }
    }

    pub fn do_redo(
        &mut self,
        actor_id: ActorID,
        seq: u32,
        message: Option<String>,
        dependencies: Clock,
    ) -> Result<Vec<Diff>, AutomergeError> {
        if let Some(redo_ops) = self.redo_stack.pop() {
            let change = Change {
                actor_id,
                seq,
                message,
                dependencies,
                operations: redo_ops,
            };
            self.undo_pos += 1;
            self.apply_change(change, false)
        } else {
            Err(AutomergeError::InvalidChange("no redo ops".to_string()))
        }
    }

    pub fn do_undo(
        &mut self,
        actor_id: ActorID,
        seq: u32,
        message: Option<String>,
        dependencies: Clock,
    ) -> Result<Vec<Diff>, AutomergeError> {
        if let Some(undo_ops) = self.undo_stack.get(self.undo_pos - 1) {
            let redo_ops = undo_ops
                .iter()
                .filter_map(|op| match &op {
                    Operation::Increment {
                        object_id: oid,
                        key,
                        value,
                    } => Some(vec![Operation::Increment {
                        object_id: oid.clone(),
                        key: key.clone(),
                        value: -value,
                    }]),
                    Operation::Set { object_id, key, .. }
                    | Operation::Link { object_id, key, .. }
                    | Operation::Delete { object_id, key } => self
                        .object_store
                        .concurrent_operations_for_field(object_id, key)
                        .map(|cops| {
                            if cops.active_op().is_some() {
                                cops.pure_operations()
                            } else {
                                vec![Operation::Delete {
                                    object_id: object_id.clone(),
                                    key: key.clone(),
                                }]
                            }
                        }),
                    _ => None,
                })
                .flatten()
                .collect();
            self.redo_stack.push(redo_ops);
            let change = Change {
                actor_id,
                seq,
                message,
                dependencies,
                operations: undo_ops.clone(),
            };
            self.undo_pos -= 1;
            self.apply_change(change, false)
        } else {
            Err(AutomergeError::InvalidChange(
                "No undo ops to execute".to_string(),
            ))
        }
    }

    /// Adds a change to the internal queue of operations, then iteratively
    /// applies all causally ready changes until there are none remaining
    ///
    /// If `make_undoable` is true, the op set will store a set of operations
    /// which can be used to undo this change.
    pub fn apply_change(
        &mut self,
        change: Change,
        make_undoable: bool,
    ) -> Result<Vec<Diff>, AutomergeError> {
        self.queue.push(change);
        let diffs = self.apply_causally_ready_changes(make_undoable)?;
        Ok(diffs)
    }

    fn apply_causally_ready_changes(
        &mut self,
        make_undoable: bool,
    ) -> Result<Vec<Diff>, AutomergeError> {
        let mut diffs = Vec::new();
        while let Some(next_change) = self.pop_next_causally_ready_change() {
            let change_diffs = self.apply_causally_ready_change(next_change, make_undoable)?;
            diffs.extend(change_diffs);
        }
        Ok(diffs)
    }

    fn pop_next_causally_ready_change(&mut self) -> Option<Change> {
        let mut index = 0;
        while index < self.queue.len() {
            let change = self.queue.get(index).unwrap();
            let deps = change.dependencies.with(&change.actor_id, change.seq - 1);
            if deps <= self.clock {
                return Some(self.queue.remove(index));
            }
            index += 1
        }
        None
    }

    fn apply_causally_ready_change(
        &mut self,
        change: Change,
        make_undoable: bool,
    ) -> Result<Vec<Diff>, AutomergeError> {
        // This method is a little more complicated than it intuitively should
        // be due to the bookkeeping required for undo. If we're asked to make
        // this operation undoable we have to store the undo operations for
        // each operation and then add them to the undo stack at the end of the
        // method. However, it's unnecessary to store undo operations for
        // objects which are created by this change (e.g if there's an insert
        // operation for a list which was created in this operation we only
        // need the undo operation for the creation of the list to achieve
        // the undo), so we track newly created objects and only store undo
        // operations which don't operate on them.
        let actor_id = change.actor_id.clone();
        let seq = change.seq;
        let operations = change.operations.clone();

        if !self.states.add_change(change)? {
            return Ok(Vec::new()); // its a duplicate - ignore
        }

        let mut diffs = Vec::new();
        let mut undo_operations = Vec::new();
        let mut new_object_ids: HashSet<ObjectID> = HashSet::new();
        for operation in operations {
            // Store newly created object IDs so we can decide whether we need
            // undo ops later
            match &operation {
                Operation::MakeMap { object_id }
                | Operation::MakeList { object_id }
                | Operation::MakeText { object_id }
                | Operation::MakeTable { object_id } => {
                    new_object_ids.insert(object_id.clone());
                }
                _ => {}
            }
            let op_with_metadata = OperationWithMetadata {
                sequence: seq,
                actor_id: actor_id.clone(),
                operation: operation.clone(),
            };
            let (diff, undo_ops_for_this_op) = self
                .object_store
                .apply_operation(&self.states, op_with_metadata)?;

            // If this object is not created in this change then we need to
            // store the undo ops for it (if we're storing undo ops at all)
            if make_undoable && !(new_object_ids.contains(operation.object_id())) {
                undo_operations.extend(undo_ops_for_this_op);
            }
            if let Some(d) = diff {
                diffs.push(d)
            }
        }
        self.clock = self.clock.with(&actor_id, seq);
        if make_undoable {
            let (new_undo_stack_slice, _) = self.undo_stack.split_at(self.undo_pos);
            let mut new_undo_stack: Vec<Vec<Operation>> = new_undo_stack_slice.to_vec();
            new_undo_stack.push(undo_operations);
            self.undo_stack = new_undo_stack;
            self.undo_pos += 1;
        };
        Ok(Self::simplify_diffs(diffs))
    }

    /// Remove any redundant diffs
    fn simplify_diffs(diffs: Vec<Diff>) -> Vec<Diff> {
        let mut result = Vec::new();
        let mut known_maxelems: HashMap<ObjectID, u32> = HashMap::new();

        for diff in diffs.into_iter().rev() {
            if let DiffAction::MaxElem(ref oid, max_elem, _) = diff.action {
                let current_max = known_maxelems.get(oid).unwrap_or(&0);
                if *current_max < max_elem {
                    known_maxelems.insert(oid.clone(), max_elem);
                    result.push(diff);
                }
            } else if let DiffAction::InsertSequenceElement(
                ref oid,
                _,
                _,
                _,
                _,
                ElementID::SpecificElementID(_, max_elem),
            ) = diff.action
            {
                let current_max = known_maxelems.get(oid).unwrap_or(&0);
                if *current_max < max_elem {
                    known_maxelems.insert(oid.clone(), max_elem);
                }
                result.push(diff);
            } else {
                result.push(diff);
            }
        }

        result.reverse();
        result
    }

    pub fn can_undo(&self) -> bool {
        self.undo_pos > 0
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Get all the changes we have that are not in `since`
    pub fn get_missing_changes(&self, since: &Clock) -> Vec<&Change> {
        self.states
            .history
            .iter()
            .map(|rc| rc.as_ref())
            .filter(|change| change.seq > since.get(&change.actor_id))
            .collect()
    }

    pub fn get_missing_deps(&self) -> Clock {
        // TODO: there's a lot of internal copying going on in here for something kinda simple
        self.queue.iter().fold(Clock::empty(), |clock, change| {
            clock
                .union(&change.dependencies)
                .with(&change.actor_id, change.seq - 1)
        })
    }
}

pub fn list_ops_in_order<'a, S: BuildHasher>(
    operations_by_elemid: &'a HashMap<ElementID, ConcurrentOperations, S>,
    following: &HashMap<ElementID, Vec<ElementID>, S>,
) -> Result<Vec<(ElementID, &'a ConcurrentOperations)>, AutomergeError> {
    // First we construct a vector of operations to process in order based
    // on the insertion orders of the operations we've received
    let mut ops_in_order: Vec<(ElementID, &ConcurrentOperations)> = Vec::new();
    // start with everything that was inserted after _head
    let mut to_process: Vec<ElementID> = following
        .get(&ElementID::Head)
        .map(|heads| {
            let mut sorted = heads.to_vec();
            sorted.sort();
            sorted
        })
        .unwrap_or_else(Vec::new);

    // for each element ID, add the operation to the ops_in_order list,
    // then find all the following element IDs, sort them and add them to
    // the list of element IDs still to process.
    while let Some(next_element_id) = to_process.pop() {
        let ops = operations_by_elemid.get(&next_element_id).ok_or_else(|| {
            AutomergeError::InvalidChange(format!(
                "Missing element ID {:?} when interpreting list ops",
                next_element_id
            ))
        })?;
        ops_in_order.push((next_element_id.clone(), ops));
        if let Some(followers) = following.get(&next_element_id) {
            let mut sorted = followers.to_vec();
            sorted.sort();
            to_process.extend(sorted);
        }
    }
    Ok(ops_in_order)
}
