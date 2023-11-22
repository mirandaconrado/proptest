//-
// Copyright 2023 The proptest developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Strategies used for abstract state machine testing.

use proptest::bits::{BitSetLike, VarBitSet};
use proptest::collection::SizeRange;
use proptest::num::sample_uniform_incl;
use proptest::std_facade::fmt::{Debug, Formatter, Result};
use proptest::std_facade::Vec;
use proptest::strategy::BoxedStrategy;
use proptest::strategy::{NewTree, Strategy, ValueTree};
use proptest::test_runner::TestRunner;

/// This trait is used to model system under test as an abstract state machine.
///
/// The key to how this works is that the set of next valid transitions depends
/// on its current state (it's not the same as generating a random sequence of
/// transitions) and just like other prop strategies, the state machine strategy
/// attempts to shrink the transitions to find the minimal reproducible example
/// when it encounters a case that breaks any of the defined properties.
///
/// This is achieved with the [`ReferenceStateMachine::transitions`] that takes
/// the current state as an argument and can be used to decide which transitions
/// are valid from this state, together with the
/// [`ReferenceStateMachine::preconditions`], which are checked during generation
/// of transitions and during shrinking.
///
/// Hence, the `preconditions` only needs to contain checks for invariants that
/// depend on the current state and may be broken by shrinking and it doesn't
/// need to cover invariants that do not depend on the current state.
///
/// The reference state machine generation runs before the generated transitions
/// are attempted to be executed against the SUT (the concrete state machine)
/// as defined by [`proptest::state_machine::StateMachineTest`].
pub trait ReferenceStateMachine {
    /// The reference state machine's state type. This should contain the minimum
    /// required information needed to implement the state machine. It is used
    /// to drive the generations of transitions to decide which transitions are
    /// valid for the current state.
    type State: Clone + Debug;

    /// The reference state machine's transition type. This is typically an enum
    /// with its variants containing the parameters required to apply the
    /// transition, if any.
    type Transition: Clone + Debug;

    // TODO Instead of the boxed strategies, this could use
    // <https://github.com/rust-lang/rust/issues/63063> once stabilized:
    // type StateStrategy = impl Strategy<Value = Self::State>;
    // type TransitionStrategy = impl Strategy<Value = Self::Transition>;

    /// The initial state may be generated by any strategy. For a constant
    /// initial state, use [`proptest::strategy::Just`].
    fn init_state() -> BoxedStrategy<Self::State>;

    /// Generate the initial transitions.
    fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition>;

    /// Apply a transition in the reference state.
    fn apply(state: Self::State, transition: &Self::Transition) -> Self::State;

    /// Pre-conditions may be specified to control which transitions are valid
    /// from the current state. If not overridden, this allows any transition.
    /// The pre-conditions are checked in the generated transitions and during
    /// shrinking.
    ///
    /// The pre-conditions checking relies on proptest global rejection
    /// filtering, which comes with some [disadvantages](https://altsysrq.github.io/proptest-book/proptest/tutorial/filtering.html).
    /// This means that pre-conditions that are hard to satisfy might slow down
    /// the test or even fail by exceeding the maximum rejection count.
    fn preconditions(
        state: &Self::State,
        transition: &Self::Transition,
    ) -> bool {
        // This is to avoid `unused_variables` warning
        let _ = (state, transition);

        true
    }

    /// A sequential strategy runs the state machine transitions generated from
    /// the reference model sequentially in a test over a concrete state, which
    /// can be implemented with the help of
    /// [`proptest::state_machine::StateMachineTest`] trait.
    ///
    /// You typically never need to override this method.
    fn sequential_strategy(
        size: impl Into<SizeRange>,
    ) -> Sequential<
        Self::State,
        Self::Transition,
        BoxedStrategy<Self::State>,
        BoxedStrategy<Self::Transition>,
    > {
        Sequential {
            size: size.into(),
            init_state: Self::init_state,
            preconditions: Self::preconditions,
            transitions: Self::transitions,
            next: Self::apply,
        }
    }
}

/// In a sequential state machine strategy, we first generate an acceptable
/// sequence of transitions. That is a sequence that satisfies the given
/// pre-conditions. The acceptability of each transition in the sequence depends
/// on the current state of the state machine, which is updated by the
/// transitions with the `next` function.
///
/// The shrinking strategy is to iteratively apply `Shrink::InitialState`,
/// `Shrink::DeleteTransition` and `Shrink::Transition`.
///
/// 1. We start by trying to delete transitions from the back of the list, until
///    we can do so no further (reached the beginning of the list).
///    We start from the back, because it's less likely to affect the state
///    machine's pre-conditions, if any.
/// 2. Then, we again iteratively attempt to shrink the individual transitions,
///    but this time starting from the front of the list - i.e. from the first
///    transition to be applied.
/// 3. Finally, we try to shrink the initial state until it's not possible to
///    shrink it any further.
///
/// For `complicate`, we attempt to undo the last shrink operation, if there was
/// any.
pub struct Sequential<State, Transition, StateStrategy, TransitionStrategy> {
    size: SizeRange,
    init_state: fn() -> StateStrategy,
    preconditions: fn(state: &State, transition: &Transition) -> bool,
    transitions: fn(state: &State) -> TransitionStrategy,
    next: fn(state: State, transition: &Transition) -> State,
}

impl<State, Transition, StateStrategy, TransitionStrategy> Debug
    for Sequential<State, Transition, StateStrategy, TransitionStrategy>
{
    fn fmt(&self, f: &mut Formatter) -> Result {
        f.debug_struct("Sequential")
            .field("size", &self.size)
            .finish()
    }
}

impl<
        State: Clone + Debug,
        Transition: Clone + Debug,
        StateStrategy: Strategy<Value = State>,
        TransitionStrategy: Strategy<Value = Transition>,
    > Strategy
    for Sequential<State, Transition, StateStrategy, TransitionStrategy>
{
    type Tree = SequentialValueTree<
        State,
        Transition,
        StateStrategy::Tree,
        TransitionStrategy::Tree,
    >;
    type Value = (State, Vec<Transition>);

    fn new_tree(&self, runner: &mut TestRunner) -> NewTree<Self> {
        // Generate the initial state value tree
        let initial_state = (self.init_state)().new_tree(runner)?;
        let last_valid_initial_state = initial_state.current();

        let (min_size, end) = self.size.start_end_incl();
        // Sample the maximum number of the transitions from the size range
        let max_size = sample_uniform_incl(runner, min_size, end);
        let mut transitions = Vec::with_capacity(max_size);
        let mut acceptable_transitions = Vec::with_capacity(max_size);
        let included_transitions = VarBitSet::saturated(max_size);
        let shrinkable_transitions = VarBitSet::saturated(max_size);

        // Sample the transitions until we reach the `max_size`
        let mut state = initial_state.current();
        while transitions.len() < max_size {
            // Apply the current state to find the current transition
            let transition_tree =
                (self.transitions)(&state).new_tree(runner)?;
            let transition = transition_tree.current();

            // If the pre-conditions are satisfied, use the transition
            if (self.preconditions)(&state, &transition) {
                transitions.push(transition_tree);
                state = (self.next)(state, &transition);
                acceptable_transitions
                    .push((TransitionState::Accepted, transition));
            } else {
                runner.reject_local("Pre-conditions were not satisfied")?;
            }
        }

        // The maximum index into the vectors and bit sets
        let max_ix = max_size - 1;

        Ok(SequentialValueTree {
            initial_state,
            is_initial_state_shrinkable: true,
            last_valid_initial_state,
            preconditions: self.preconditions,
            next: self.next,
            transitions,
            acceptable_transitions,
            included_transitions,
            shrinkable_transitions,
            max_ix,
            // On a failure, we start by shrinking transitions from the back
            // which is less likely to invalidate pre-conditions
            shrink: Shrink::DeleteTransition(max_ix),
            last_shrink: None,
        })
    }
}

/// A shrinking operation
#[derive(Clone, Copy, Debug)]
enum Shrink {
    /// Shrink the initial state
    InitialState,
    /// Delete a transition at given index
    DeleteTransition(usize),
    /// Shrink a transition at given index
    Transition(usize),
}
use Shrink::*;

/// The state of a transition in the model
#[derive(Clone, Copy, Debug)]
enum TransitionState {
    /// The transition that is equal to the result of `ValueTree::current()`
    /// and satisfies the pre-conditions
    Accepted,
    /// The transition has been simplified, but rejected by pre-conditions
    SimplifyRejected,
    /// The transition has been complicated, but rejected by pre-conditions
    ComplicateRejected,
}
use TransitionState::*;

/// The generated value tree for a sequential state machine.
pub struct SequentialValueTree<
    State,
    Transition,
    StateValueTree,
    TransitionValueTree,
> {
    /// The initial state value tree
    initial_state: StateValueTree,
    /// Can the `initial_state` be shrunk any further?
    is_initial_state_shrinkable: bool,
    /// The last initial state that has been accepted by the pre-conditions.
    /// We have to store this every time before attempt to shrink to be able
    /// to back to it in case the shrinking is rejected.
    last_valid_initial_state: State,
    /// The pre-conditions predicate
    preconditions: fn(&State, &Transition) -> bool,
    /// The function from current state and a transition to an updated state
    next: fn(State, &Transition) -> State,
    /// The list of transitions' value trees
    transitions: Vec<TransitionValueTree>,
    /// The sequence of included transitions with their shrinking state
    acceptable_transitions: Vec<(TransitionState, Transition)>,
    /// The bit-set of transitions that have not been deleted by shrinking
    included_transitions: VarBitSet,
    /// The bit-set of transitions that can be shrunk further
    shrinkable_transitions: VarBitSet,
    /// The maximum index in the `transitions` vector (its size - 1)
    max_ix: usize,
    /// The next shrink operation to apply
    shrink: Shrink,
    /// The last applied shrink operation, if any
    last_shrink: Option<Shrink>,
}

impl<
        State: Clone + Debug,
        Transition: Clone + Debug,
        StateValueTree: ValueTree<Value = State>,
        TransitionValueTree: ValueTree<Value = Transition>,
    >
    SequentialValueTree<State, Transition, StateValueTree, TransitionValueTree>
{
    /// Try to apply the next `self.shrink`. Returns `true` if a shrink has been
    /// applied.
    fn try_simplify(&mut self) -> bool {
        if let DeleteTransition(ix) = self.shrink {
            // Delete the index from the included transitions
            self.included_transitions.clear(ix);

            self.last_shrink = Some(self.shrink);
            self.shrink = if ix == 0 {
                // Reached the beginning of the list, move on to shrinking
                Transition(0)
            } else {
                // Try to delete the previous transition next
                DeleteTransition(ix - 1)
            };
            // If this delete is not acceptable, undo it and try again
            if !self.check_acceptable(None) {
                self.included_transitions.set(ix);
                self.last_shrink = None;
                return self.try_simplify();
            }
            // If the delete was accepted, remove this index from shrinkable
            // transitions
            self.shrinkable_transitions.clear(ix);
            return true;
        }

        while let Transition(ix) = self.shrink {
            if self.shrinkable_transitions.count() == 0 {
                // Move on to shrinking the initial state
                self.shrink = Shrink::InitialState;
                break;
            }

            if !self.included_transitions.test(ix) {
                // No use shrinking something we're not including
                self.shrink = self.next_shrink_transition(ix);
                continue;
            }

            if let Some((SimplifyRejected, _trans)) =
                self.acceptable_transitions.get(ix)
            {
                // This transition is already simplified and rejected
                self.shrink = self.next_shrink_transition(ix);
            } else if self.transitions[ix].simplify() {
                self.last_shrink = Some(self.shrink);
                if self.check_acceptable(Some(ix)) {
                    self.acceptable_transitions[ix] =
                        (Accepted, self.transitions[ix].current());
                    return true;
                } else {
                    let (state, _trans) =
                        self.acceptable_transitions.get_mut(ix).unwrap();
                    *state = SimplifyRejected;
                    self.shrinkable_transitions.clear(ix);
                    self.shrink = self.next_shrink_transition(ix);
                    return self.simplify();
                }
            } else {
                self.shrinkable_transitions.clear(ix);
                self.shrink = self.next_shrink_transition(ix);
            }
        }

        if let InitialState = self.shrink {
            if self.initial_state.simplify() {
                if self.check_acceptable(None) {
                    // Store the valid initial state
                    self.last_valid_initial_state =
                        self.initial_state.current();
                    return true;
                } else {
                    // If the shrink is not acceptable, clear it out
                    self.last_shrink = None;
                }
            }
            self.is_initial_state_shrinkable = false;
            // Nothing left to do
            return false;
        }

        // This statement should never be reached
        panic!("Unexpected shrink state");
    }

    /// Find if there's any acceptable included transition that is not current,
    /// starting from the given index. Expects that all the included transitions
    /// are currently being rejected (when `can_simplify` returns `false`).
    fn try_to_find_acceptable_transition(&mut self, ix: usize) -> bool {
        let mut ix_to_check = ix;
        loop {
            if self.included_transitions.test(ix_to_check)
                && self.check_acceptable(Some(ix_to_check))
            {
                self.acceptable_transitions[ix_to_check] =
                    (Accepted, self.transitions[ix_to_check].current());
                return true;
            }
            // Move on to the next transition
            if ix_to_check == self.max_ix {
                ix_to_check = 0;
            } else {
                ix_to_check += 1;
            }
            // We're back to where we started, there nothing left to do
            if ix_to_check == ix {
                return false;
            }
        }
    }

    /// Check if the sequence of included transitions is acceptable by the
    /// pre-conditions. When `ix` is not `None`, the transition at the given
    /// index is taken from its current value.
    fn check_acceptable(&self, ix: Option<usize>) -> bool {
        let transitions = self.get_included_acceptable_transitions(ix);
        let mut state = self.last_valid_initial_state.clone();
        for transition in transitions.iter() {
            let is_acceptable = (self.preconditions)(&state, transition);
            if is_acceptable {
                state = (self.next)(state, transition);
            } else {
                return false;
            }
        }
        true
    }

    /// The currently included and acceptable transitions. When `ix` is not
    /// `None`, the transition at this index is taken from its current value
    /// which may not be acceptable by the pre-conditions, instead of its
    /// acceptable value.
    fn get_included_acceptable_transitions(
        &self,
        ix: Option<usize>,
    ) -> Vec<Transition> {
        self.acceptable_transitions
            .iter()
            .enumerate()
            // Filter out deleted transitions
            .filter(|&(this_ix, _)| self.included_transitions.test(this_ix))
            // Map the indices to the values
            .map(|(this_ix, (_, transition))| match ix {
                Some(ix) if this_ix == ix => self.transitions[ix].current(),
                _ => transition.clone(),
            })
            .collect()
    }

    /// Find if the initial state is still shrinkable or if any of the
    /// simplifications and complications of the included transitions have not
    /// yet been rejected.
    fn can_simplify(&self) -> bool {
        self.is_initial_state_shrinkable ||
             // If there are some transitions whose shrinking has not yet been 
             // rejected, we can try to shrink them further
             !self
                .acceptable_transitions
                .iter()
                .enumerate()
                // Filter out deleted transitions
                .filter(|&(ix, _)| self.included_transitions.test(ix))
                .all(|(_, (state, _transition))| {
                    matches!(state, SimplifyRejected | ComplicateRejected)
                })
    }

    /// Find the next shrink transition. Loops back to the front of the list
    /// when the end is reached, because sometimes a transition might become
    /// acceptable only after a transition that comes before it in the sequence
    /// gets shrunk.
    fn next_shrink_transition(&self, current_ix: usize) -> Shrink {
        if current_ix == self.max_ix {
            // Either loop back to the start of the list...
            Transition(0)
        } else {
            // ...or move on to the next transition
            Transition(current_ix + 1)
        }
    }
}

impl<
        State: Clone + Debug,
        Transition: Clone + Debug,
        StateValueTree: ValueTree<Value = State>,
        TransitionValueTree: ValueTree<Value = Transition>,
    > ValueTree
    for SequentialValueTree<
        State,
        Transition,
        StateValueTree,
        TransitionValueTree,
    >
{
    type Value = (State, Vec<Transition>);

    fn current(&self) -> Self::Value {
        (
            self.last_valid_initial_state.clone(),
            // The current included acceptable transitions
            self.get_included_acceptable_transitions(None),
        )
    }

    fn simplify(&mut self) -> bool {
        if self.can_simplify() {
            self.try_simplify()
        } else {
            if let Some(Transition(ix)) = self.last_shrink {
                return self.try_to_find_acceptable_transition(ix);
            }
            false
        }
    }

    fn complicate(&mut self) -> bool {
        match self.last_shrink {
            None => false,
            Some(DeleteTransition(ix)) => {
                // Undo the last item we deleted. Can't complicate any further,
                // so unset prev_shrink.
                self.included_transitions.set(ix);
                self.shrinkable_transitions.set(ix);
                self.last_shrink = None;
                true
            }
            Some(Transition(ix)) => {
                if self.transitions[ix].complicate() {
                    if self.check_acceptable(Some(ix)) {
                        self.acceptable_transitions[ix] =
                            (Accepted, self.transitions[ix].current());
                        // Don't unset prev_shrink; we may be able to complicate
                        // it again
                        return true;
                    } else {
                        let (state, _trans) =
                            self.acceptable_transitions.get_mut(ix).unwrap();
                        *state = ComplicateRejected;
                    }
                }
                // Can't complicate the last element any further
                self.last_shrink = None;
                false
            }
            Some(InitialState) => {
                self.last_shrink = None;
                if self.initial_state.complicate()
                    && self.check_acceptable(None)
                {
                    self.last_valid_initial_state =
                        self.initial_state.current();
                    // Don't unset prev_shrink; we may be able to complicate
                    // it again
                    return true;
                }
                // Can't complicate the initial state any further
                self.last_shrink = None;
                false
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use proptest::collection::hash_set;
    use proptest::prelude::*;

    use heap_state_machine::*;
    use std::collections::HashSet;

    /// A number of simplifications that can be applied in the `ValueTree`
    /// produced by [`deterministic_sequential_value_tree`]. It depends on the
    /// [`TRANSITIONS`] given to its `sequential_strategy`.
    ///
    /// This constant can be determined from the test
    /// `number_of_sequential_value_tree_simplifications`.
    const SIMPLIFICATIONS: usize = 32;
    /// Number of transitions in the [`deterministic_sequential_value_tree`].
    const TRANSITIONS: usize = 32;

    #[test]
    fn number_of_sequential_value_tree_simplifications() {
        let mut value_tree = deterministic_sequential_value_tree();

        let mut i = 0;
        loop {
            let simplified = value_tree.simplify();
            if simplified {
                i += 1;
            } else {
                break;
            }
        }
        assert_eq!(i, SIMPLIFICATIONS);
    }

    proptest! {
        /// Test the simplifications and complication of the
        /// `SequentialValueTree` produced by
        /// `deterministic_sequential_value_tree`.
        ///
        /// The indices of simplification on which we'll attempt to complicate
        /// after simplification are selected from the randomly generated
        /// `complicate_ixs`.
        ///
        /// Every simplification and complication must satisfy pre-conditions of
        /// the state-machine.
        #[test]
        fn test_state_machine_sequential_value_tree(
            complicate_ixs in hash_set(0..SIMPLIFICATIONS, 0..SIMPLIFICATIONS)
        ) {
            test_state_machine_sequential_value_tree_aux(complicate_ixs)
        }
    }

    fn test_state_machine_sequential_value_tree_aux(
        complicate_ixs: HashSet<usize>,
    ) {
        println!("Complicate indices: {complicate_ixs:?}");

        let mut value_tree = deterministic_sequential_value_tree();

        let check_preconditions = |value_tree: &TestValueTree| {
            let (mut state, transitions) = value_tree.current();
            let len = transitions.len();
            println!("Transitions {}", len);
            for (ix, transition) in transitions.into_iter().enumerate() {
                println!("Transition {}/{len} {transition:?}", ix + 1);
                // Every transition must satisfy the pre-conditions
                assert!(
                    <HeapStateMachine as ReferenceStateMachine>::preconditions(
                        &state,
                        &transition
                    )
                );

                // Apply the transition to update the state for the next transition
                state = <HeapStateMachine as ReferenceStateMachine>::apply(
                    state,
                    &transition,
                );
            }
        };

        let mut ix = 0_usize;
        loop {
            let simplified = value_tree.simplify();

            check_preconditions(&value_tree);

            if !simplified {
                break;
            }
            ix += 1;

            if complicate_ixs.contains(&ix) {
                loop {
                    let complicated = value_tree.complicate();

                    check_preconditions(&value_tree);

                    if !complicated {
                        break;
                    }
                }
            }
        }
    }

    /// The following is a definition of an reference state machine used for the
    /// tests.
    mod heap_state_machine {
        use std::vec::Vec;

        use crate::{ReferenceStateMachine, SequentialValueTree};
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;

        use super::TRANSITIONS;

        pub struct HeapStateMachine;

        pub type TestValueTree = SequentialValueTree<
            TestState,
            TestTransition,
            <BoxedStrategy<TestState> as Strategy>::Tree,
            <BoxedStrategy<TestTransition> as Strategy>::Tree,
        >;

        pub type TestState = Vec<i32>;

        #[derive(Clone, Debug)]
        pub enum TestTransition {
            PopNonEmpty,
            PopEmpty,
            Push(i32),
        }

        pub fn deterministic_sequential_value_tree() -> TestValueTree {
            let sequential =
                <HeapStateMachine as ReferenceStateMachine>::sequential_strategy(
                    TRANSITIONS,
                );
            let mut runner = TestRunner::deterministic();
            sequential.new_tree(&mut runner).unwrap()
        }

        impl ReferenceStateMachine for HeapStateMachine {
            type State = TestState;
            type Transition = TestTransition;

            fn init_state() -> BoxedStrategy<Self::State> {
                Just(vec![]).boxed()
            }

            fn transitions(
                state: &Self::State,
            ) -> BoxedStrategy<Self::Transition> {
                if state.is_empty() {
                    prop_oneof![
                        1 => Just(TestTransition::PopEmpty),
                        2 => (any::<i32>()).prop_map(TestTransition::Push),
                    ]
                    .boxed()
                } else {
                    prop_oneof![
                        1 => Just(TestTransition::PopNonEmpty),
                        2 => (any::<i32>()).prop_map(TestTransition::Push),
                    ]
                    .boxed()
                }
            }

            fn apply(
                mut state: Self::State,
                transition: &Self::Transition,
            ) -> Self::State {
                match transition {
                    TestTransition::PopEmpty => {
                        state.pop();
                    }
                    TestTransition::PopNonEmpty => {
                        state.pop();
                    }
                    TestTransition::Push(value) => state.push(*value),
                }
                state
            }

            fn preconditions(
                state: &Self::State,
                transition: &Self::Transition,
            ) -> bool {
                match transition {
                    TestTransition::PopEmpty => state.is_empty(),
                    TestTransition::PopNonEmpty => !state.is_empty(),
                    TestTransition::Push(_) => true,
                }
            }
        }
    }
}
