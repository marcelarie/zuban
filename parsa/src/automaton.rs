use std::collections::{HashMap, HashSet};
use std::fmt;
use std::iter::repeat;
use std::pin::Pin;
use std::hash::BuildHasherDefault;

use fnv::FnvHashMap;

pub const NODE_START: u16 = 1 << 15;
pub const ERROR_RECOVERY_BIT: u16 = 1 << 14;

type SquashedTransitions = FastHashMap<InternalSquashedType, Plan>;
pub type Automatons = FastHashMap<InternalNonterminalType, RuleAutomaton>;
pub type InternalStrToToken = FastHashMap<&'static str, InternalTerminalType>;
pub type InternalStrToNode = FastHashMap<&'static str, InternalNonterminalType>;
pub type RuleMap = FastHashMap<InternalNonterminalType, (&'static str, Rule)>;
pub type SoftKeywords = FastHashMap<InternalTerminalType, HashSet<&'static str>>;
type FirstPlans = FastHashMap<InternalNonterminalType, FirstPlan>;
type DFAStates = Vec<Pin<Box<DFAState>>>;
pub type FastHashMap<K, V> = FnvHashMap<K, V>;

pub fn new_fast_hash_map<K, V>() -> FastHashMap<K, V> {
    FnvHashMap::default()
}

#[derive(Debug)]
pub enum Rule {
    Identifier(&'static str),
    Keyword(&'static str),
    Or(&'static Rule, &'static Rule),
    Cut(&'static Rule, &'static Rule),
    Maybe(&'static Rule),
    Multiple(&'static Rule),
    NegativeLookahead(&'static Rule),
    PositiveLookahead(&'static Rule),
    Next(&'static Rule, &'static Rule),
    NodeMayBeOmitted(&'static Rule),
    DoesErrorRecovery(&'static Rule),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct InternalSquashedType(pub u16);

impl InternalSquashedType {
    #[inline]
    pub fn is_leaf(&self) -> bool {
        self.0 < NODE_START
    }

    #[inline]
    pub fn is_error_recovery(&self) -> bool {
        self.0 & ERROR_RECOVERY_BIT > 0
    }

    #[inline]
    pub fn remove_error_recovery_bit(&self) -> Self {
        Self(self.0 & !ERROR_RECOVERY_BIT)
    }

    #[inline]
    pub fn set_error_recovery_bit(&self) -> Self {
        Self(self.0 | ERROR_RECOVERY_BIT)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct InternalNonterminalType(pub u16);
impl InternalNonterminalType {
    #[inline]
    pub fn to_squashed(&self) -> InternalSquashedType {
        InternalSquashedType(self.0)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct InternalTerminalType(pub u16);
impl InternalTerminalType {
    #[inline]
    pub fn to_squashed(&self) -> InternalSquashedType {
        InternalSquashedType(self.0)
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Default)]
struct NFAStateId(usize);

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct DFAStateId(pub usize);

// NFA = nondeterministic finite automaton
#[derive(Debug)]
struct NFAState {
    transitions: Vec<NFATransition>,
}

// DFA = deterministic finite automaton
#[derive(Debug)]
pub struct DFAState {
    transitions: Vec<DFATransition>,
    nfa_set: HashSet<NFAStateId>,
    pub is_final: bool,
    is_calculated: bool,
    pub node_may_be_omitted: bool,
    list_index: DFAStateId, // The index in the dfa_states vec in the automaton.

    // This is the important part that will be used by the parser. The rest is
    // just there to generate this information.
    pub transition_to_plan: SquashedTransitions,
    pub from_rule: &'static str,
}

// Safe, because dfas are behind a pinned box that never gets changed
unsafe impl Sync for DFAState {}
unsafe impl Send for DFAState {}

#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
enum TransitionType {
    Terminal(InternalTerminalType, &'static str),
    Nonterminal(InternalNonterminalType),
    Keyword(&'static str),
    PositiveLookaheadStart,
    NegativeLookaheadStart,
    LookaheadEnd,
}

#[derive(Debug)]
struct NFATransition {
    type_: Option<TransitionType>,
    to: NFAStateId,
}

#[derive(Debug, Clone, Copy)]
struct DFATransition {
    type_: TransitionType,
    to: *mut DFAState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StackMode {
    PositiveLookahead,
    Alternative(*const Plan),
    Normal,
}

#[derive(Debug, Clone)]
pub struct Push {
    pub node_type: InternalNonterminalType,
    pub next_dfa: *const DFAState,
    pub stack_mode: StackMode,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub pushes: Vec<Push>,
    pub next_dfa: *const DFAState,
    pub type_: InternalSquashedType,
    pub is_left_recursive: bool,
    pub debug_text: &'static str,
}

// Safe, because plan pointers are behind a pinned box that never gets changed
unsafe impl Sync for Plan {}
unsafe impl Send for Plan {}

enum FirstPlan {
    Calculated(SquashedTransitions, bool),
    Calculating,
}

#[derive(Debug, Default)]
pub struct Keywords {
    counter: usize,
    keywords: FastHashMap<&'static str, InternalSquashedType>,
}

impl Keywords {
    fn add(&mut self, keyword: &'static str) {
        if !self.keywords.contains_key(keyword) {
            self.keywords
                .insert(keyword, Self::keyword_to_squashed(self.counter));
            self.counter += 1;
        }
    }

    pub fn keyword_to_squashed(number: usize) -> InternalSquashedType {
        InternalSquashedType(number as u16)
    }

    pub fn get_squashed(&self, keyword: &str) -> Option<InternalSquashedType> {
        self.keywords.get(keyword).copied()
    }
}

#[derive(Default, Debug)]
pub struct RuleAutomaton {
    pub type_: InternalNonterminalType,
    nfa_states: Vec<NFAState>,
    pub dfa_states: DFAStates,
    name: &'static str,
    node_may_be_omitted: bool,
    nfa_end_id: NFAStateId,
    no_transition_dfa_id: Option<DFAStateId>,
    fallback_plans: Vec<Pin<Box<Plan>>>,
    pub does_error_recovery: bool,
}

impl RuleAutomaton {
    fn build(
        &mut self,
        nonterminal_map: &InternalStrToNode,
        terminal_map: &InternalStrToToken,
        keywords: &mut Keywords,
        rule: &Rule,
    ) -> (NFAStateId, NFAStateId) {
        let mut build = |automaton: &mut Self, rule| {
            automaton.build(nonterminal_map, terminal_map, keywords, rule)
        };
        use Rule::*;
        match *rule {
            Identifier(string) => {
                let (start, end) = self.new_nfa_states();
                if let Some(&t) = terminal_map.get(string) {
                    self.add_transition(start, end, Some(TransitionType::Terminal(t, string)));
                } else if let Some(&t) = nonterminal_map.get(string) {
                    self.add_transition(start, end, Some(TransitionType::Nonterminal(t)));
                } else {
                    panic!("No terminal / nonterminal found for {:?}; token_map = {:?}; node_map ={:?}",
                           string, terminal_map, nonterminal_map);
                }
                (start, end)
            }
            Keyword(string) => {
                let (start, end) = self.new_nfa_states();
                self.add_transition(start, end, Some(TransitionType::Keyword(string)));
                keywords.add(string);
                (start, end)
            }
            Or(rule1, rule2) => {
                let (start, end) = self.new_nfa_states();
                for r in [rule1, rule2].iter() {
                    let (x, y) = build(self, r);
                    self.add_empty_transition(start, x);
                    self.add_empty_transition(y, end);
                }
                (start, end)
            }
            Maybe(rule1) => {
                let (start, end) = build(self, rule1);
                self.add_empty_transition(start, end);
                (start, end)
            }
            Multiple(rule1) => {
                let (start, end) = build(self, rule1);
                self.add_empty_transition(end, start);
                (start, end)
            }
            NegativeLookahead(rule1) => {
                let (start, end) = build(self, rule1);
                let (new_start, new_end) = self.new_nfa_states();
                self.add_transition(
                    new_start,
                    start,
                    Some(TransitionType::NegativeLookaheadStart),
                );
                self.add_transition(end, new_end, Some(TransitionType::LookaheadEnd));
                (new_start, new_end)
            }
            PositiveLookahead(rule1) => {
                let (start, end) = build(self, rule1);
                let (new_start, new_end) = self.new_nfa_states();
                self.add_transition(
                    new_start,
                    start,
                    Some(TransitionType::PositiveLookaheadStart),
                );
                self.add_transition(end, new_end, Some(TransitionType::LookaheadEnd));
                (new_start, new_end)
            }
            // TODO Cut is ignored for now.
            Cut(rule1, rule2) => {
                unimplemented!()
            }
            Next(rule1, rule2) => {
                let (start1, end1) = build(self, rule1);
                let (start2, end2) = build(self, rule2);
                self.add_empty_transition(end1, start2);
                (start1, end2)
            }
            NodeMayBeOmitted(rule1) => {
                self.node_may_be_omitted = true;
                build(self, rule1)
            }
            DoesErrorRecovery(rule) => {
                self.does_error_recovery = true;
                build(self, rule)
            }
        }
    }

    fn get_nfa_state_mut(&mut self, id: NFAStateId) -> &mut NFAState {
        &mut self.nfa_states[id.0]
    }

    fn get_nfa_state(&self, id: NFAStateId) -> &NFAState {
        &self.nfa_states[id.0]
    }

    fn new_nfa_states(&mut self) -> (NFAStateId, NFAStateId) {
        let mut new = || {
            self.nfa_states.push(NFAState {
                transitions: Default::default(),
            });
            NFAStateId(self.nfa_states.len() - 1)
        };
        (new(), new())
    }

    fn add_transition(&mut self, start: NFAStateId, to: NFAStateId, type_: Option<TransitionType>) {
        self.get_nfa_state_mut(start)
            .transitions
            .push(NFATransition { type_, to });
    }

    fn add_empty_transition(&mut self, start: NFAStateId, to: NFAStateId) {
        self.add_transition(start, to, None);
    }

    fn group_nfas(&self, nfa_state_ids: Vec<NFAStateId>) -> HashSet<NFAStateId> {
        // Group all NFAs that are ε-moves (which are essentially transitions with None)
        let mut set: HashSet<_> = nfa_state_ids.iter().cloned().collect();
        for nfa_state_id in &nfa_state_ids {
            for transition in &self.get_nfa_state(*nfa_state_id).transitions {
                // Mode changes need to have separate DFA states as well.
                if transition.type_ == None {
                    set.insert(transition.to);
                    if !nfa_state_ids.contains(&transition.to) {
                        set.extend(self.group_nfas(set.iter().cloned().collect()));
                    }
                }
            }
        }
        set
    }

    fn nfa_to_dfa(&mut self, starts: Vec<NFAStateId>, end: NFAStateId) -> *mut DFAState {
        // Since we have the intial `starts` grouped by the mode change, we can
        // now just check for all ε-transitions that have no mode change.
        let grouped_nfas = self.group_nfas(starts);
        for (i, dfa_state) in self.dfa_states.iter_mut().enumerate() {
            if dfa_state.nfa_set == grouped_nfas {
                return dfa_state as &mut DFAState;
            }
        }
        let is_final = grouped_nfas.contains(&end)
            || grouped_nfas
                .iter()
                .any(|nfa_id| self.get_nfa_state(*nfa_id).is_lookahead_end()
                     );//|| self.get_nfa_state(*nfa_id).has_negative_lookahead_at_end(&self.nfa_states, self.nfa_end_id));
        self.dfa_states.push(Pin::new(Box::new(DFAState {
            nfa_set: grouped_nfas,
            is_final,
            is_calculated: false,
            list_index: DFAStateId(self.dfa_states.len()),
            node_may_be_omitted: self.node_may_be_omitted,
            from_rule: self.name,
            transition_to_plan: Default::default(),
            transitions: Default::default(),
        })));
        self.dfa_states.last_mut().unwrap() as &mut DFAState
    }

    fn construct_powerset(&mut self, start: NFAStateId, end: NFAStateId) {
        let dfa = self.nfa_to_dfa(vec![start], end);
        self.construct_powerset_for_dfa(dfa, end);

        for dfa in &mut self.dfa_states {
            if dfa.transitions.iter().any(
                |t| t.type_ == TransitionType::NegativeLookaheadStart && {
                    let end = search_lookahead_end(unsafe {&*t.to});
                    unsafe {&*end}.is_final
                }
            ) {
                dfa.is_final = true;
            }
        }
    }

    fn construct_powerset_for_dfa(&mut self, dfa: *mut DFAState, end: NFAStateId) {
        // Safe because DFAs are pinnned boxes in insert only lists
        let state = unsafe { &mut *dfa };
        if state.is_calculated {
            return;
        }

        let mut grouped_transitions = new_fast_hash_map::<_, Vec<NFAStateId>>();
        let mut nfa_list: Vec<NFAStateId> = state.nfa_set.iter().cloned().collect();
        // Need to sort the list by ID to make sure that the lower IDs have higher priority. The
        // rules always generate NFAStates in order of priority.
        nfa_list.sort_by_key(|x| x.0);
        for nfa_state_id in nfa_list {
            let n = &self.get_nfa_state(nfa_state_id);
            for transition in &n.transitions {
                // The nodes that have no proper type are only interesting if there's a mode
                // change.
                if let Some(t) = transition.type_ {
                    let t = transition.type_.unwrap();
                    match grouped_transitions.get_mut(&(t)).and_then(|x| {
                        if transition.is_terminal_nonterminal_or_keyword() {
                            Some(x)
                        } else {
                            None
                        }
                    }) {
                        Some(v) => v.push(transition.to),
                        None => {
                            grouped_transitions.insert(t, vec![transition.to]);
                        }
                    }
                }
            }
        }

        let mut transitions = Vec::new();
        for (type_, grouped_starts) in grouped_transitions {
            let new_dfa_id = self.nfa_to_dfa(grouped_starts, end);
            transitions.push(DFATransition {
                type_,
                to: new_dfa_id,
            });
        }

        state.transitions = transitions;
        state.is_calculated = true;
        let transitions = state.transitions.clone();
        for transition in transitions {
            self.construct_powerset_for_dfa(transition.to, end)
        }
    }

    fn add_no_transition_dfa_if_necessary(&mut self) {
        if self.nfa_states.iter().any(|nfa| nfa.transitions.iter().any(
            |t| t.type_ == Some(TransitionType::NegativeLookaheadStart)
        )) {
            let list_index = DFAStateId(self.dfa_states.len());
            self.dfa_states.push(Box::pin(DFAState {
                nfa_set: HashSet::new(),
                is_final: false,
                is_calculated: true,
                list_index,
                node_may_be_omitted: self.node_may_be_omitted,
                from_rule: self.name,
                transition_to_plan: Default::default(),
                transitions: Default::default(),
            }));
            self.no_transition_dfa_id = Some(list_index);
        }
    }

    pub fn illustrate_dfas(&self, nonterminal_map: &InternalStrToNode) -> String {
        // Sorry for this code, it's really ugly, but since it's really only for debugging
        // purposes, I don't care too much. ~dave
        let format_index = |id: usize, dfa: &DFAState| {
            (id + 1).to_string() + (if dfa.is_final { " (final)" } else { "" })
        };
        let mut out_strings = vec![];
        let mut transition_list = vec![];
        let mut first_line = vec![format_index(0, &self.dfa_states[0]), "#".to_owned()];
        first_line.extend(
            repeat("o".to_owned())
                .take(self.dfa_states[0].transitions.len())
                .collect::<Vec<_>>(),
        );
        out_strings.push(first_line);
        for (i, dfa) in self.dfa_states.iter().enumerate() {
            if i + 1 == self.dfa_states.len() {
                // Was already displayed.
                break;
            }

            while transition_list.last() == Some(&None) {
                transition_list.pop();
            }
            for t in &dfa.transitions {
                transition_list.push(Some((
                    t.get_next_dfa().list_index,
                    match t.type_ {
                        TransitionType::Terminal(_, s) => s.to_owned(),
                        TransitionType::Nonterminal(t) => {
                            nonterminal_to_str(nonterminal_map, t).to_owned()
                        }
                        TransitionType::Keyword(s) => {
                            format!("{:#?}", s)
                        }
                        TransitionType::PositiveLookaheadStart => "POS_LOOK".to_owned(),
                        TransitionType::LookaheadEnd => "LOOK_END".to_owned(),
                        TransitionType::NegativeLookaheadStart => "NEG_LOOK".to_owned(),
                    },
                )));
            }

            let mut v1 = vec!["".to_owned(), "#".to_owned()];
            let mut v2 = vec!["".to_owned(), "#".to_owned()];
            let mut v3 = vec!["".to_owned(), "#".to_owned()];
            let mut v4 = vec![format_index(i + 1, &self.dfa_states[i + 1]), "#".to_owned()];
            let len = transition_list.len();
            for t in transition_list.iter_mut() {
                if let Some((to, s)) = t.clone() {
                    v1.push("|".to_owned());
                    v2.push(if s.is_empty() { "|".to_owned() } else { s });
                    t.replace((to, "".to_owned()));
                    v3.push(if to.0 <= i + 1 {
                        t.take();
                        if to.0 == i + 1 {
                            "|".to_owned()
                        } else {
                            format!("-> {}", to.0 + 1)
                        }
                    } else {
                        "|".to_owned()
                    });
                    v4.push(
                        (if to.0 == i + 1 {
                            "o"
                        } else if to.0 <= i {
                            ""
                        } else {
                            "|"
                        })
                        .to_owned(),
                    );
                } else {
                    v1.push("".to_owned());
                    v2.push("".to_owned());
                    v3.push("".to_owned());
                    v4.push("".to_owned());
                }
            }
            out_strings.push(v1);
            out_strings.push(v2);
            out_strings.push(v3);
            out_strings.push(v4);
        }
        let mut column_widths = vec![];
        for line in &out_strings {
            for (i, field) in line.iter().enumerate() {
                match column_widths.get(i) {
                    None => column_widths.push(field.len()),
                    Some(f) => {
                        if column_widths[i] < field.len() {
                            column_widths[i] = field.len()
                        }
                    }
                };
            }
        }
        let mut s = String::new();
        for line in &out_strings {
            for (field, max_width) in line.iter().zip(&column_widths) {
                s += &format!("{:^width$}", field, width = max_width + 2);
            }
            s += "\n";
        }
        s
    }
}

impl NFAState {
    fn is_lookahead_end(&self) -> bool {
        self.transitions
            .iter()
            .any(|t| t.type_ == Some(TransitionType::LookaheadEnd))
    }

    fn has_negative_lookahead_at_end(&self, nfa_states: &[NFAState], nfa_end_id: NFAStateId) -> bool {
        fn is_final(nfa_states: &[NFAState], nfa_end_id: NFAStateId, nfa_state: &NFAState) -> bool {
            // This implementation is very simplistic for now. At the moment negative lookaheads
            // may also only be simple terminals.
            dbg!(&nfa_state.transitions);
            nfa_state.transitions.iter().any(
                |t| t.to == nfa_end_id || match t.type_ {
                    None => is_final(nfa_states, nfa_end_id, &nfa_states[t.to.0]),
                    Some(_) => false
                }
            )
        };
        self.transitions
            .iter()
            .any(|t| {
                if t.type_ == Some(TransitionType::NegativeLookaheadStart) {
                    let lookahead = &nfa_states[t.to.0];
                    assert!(lookahead.transitions.len() == 1);
                    let lookahead_end = &nfa_states[lookahead.transitions[0].to.0];
                    assert!(lookahead_end.transitions.len() == 1);
                    assert!(lookahead_end.transitions[0].type_ == Some(TransitionType::LookaheadEnd));
                    let maybe_final = &nfa_states[lookahead_end.transitions[0].to.0];
                    dbg!(&lookahead.transitions[0]);
                    let x= is_final(nfa_states, nfa_end_id, &maybe_final);
                    dbg!("X", nfa_end_id == lookahead.transitions[0].to, x);
                    x
                } else {
                    false
                }
            })
    }
}

impl DFAState {
    fn is_lookahead_end(&self) -> bool {
        self.transitions
            .iter()
            .any(|t| t.type_ == TransitionType::LookaheadEnd)
    }

    pub fn get_nonterminal_transition_ids(&self) -> Vec<InternalNonterminalType> {
        let mut transition_ids = vec![];
        for transition in &self.transitions {
            if let TransitionType::Nonterminal(id) = transition.type_ {
                transition_ids.push(id);
            }
        }
        transition_ids
    }
}

impl NFATransition {
    fn is_terminal_nonterminal_or_keyword(&self) -> bool {
        self.type_.map_or(false, |t| {
            matches!(
                t,
                TransitionType::Nonterminal(_)
                    | TransitionType::Terminal(_, _)
                    | TransitionType::Keyword(_)
            )
        })
    }
}

impl DFATransition {
    pub fn get_next_dfa(&self) -> &DFAState {
        unsafe { &*self.to }
    }
}

impl Plan {
    pub fn get_next_dfa(&self) -> &DFAState {
        unsafe { &*self.next_dfa }
    }
}

pub fn generate_automatons(
    nonterminal_map: &InternalStrToNode,
    terminal_map: &InternalStrToToken,
    rules: &RuleMap,
    soft_keywords: &SoftKeywords,
) -> (Automatons, Keywords) {
    let mut keywords = Keywords {
        // We need to start the numbers of keywords after tokens. Keyword ID's therefore never
        // clash with Token IDs (both are of type SquashedInternalType).
        counter: terminal_map.len(),
        ..Default::default()
    };
    let mut automatons = new_fast_hash_map();
    let dfa_counter = 0;
    for (internal_type, (rule_name, rule)) in rules {
        let mut automaton = RuleAutomaton {
            type_: *internal_type,
            name: rule_name,
            ..Default::default()
        };
        let (start, end) = automaton.build(nonterminal_map, terminal_map, &mut keywords, rule);
        automaton.nfa_end_id = end;
        automaton.construct_powerset(start, end);
        automaton.add_no_transition_dfa_if_necessary();
        automatons.insert(*internal_type, automaton);
    }

    // Calculate first plans
    let mut first_plans = new_fast_hash_map();
    let rule_labels = automatons
        .keys()
        .cloned()
        .collect::<Vec<InternalNonterminalType>>();
    for rule_label in &rule_labels {
        create_first_plans(
            nonterminal_map,
            &keywords,
            soft_keywords,
            &mut first_plans,
            &mut automatons,
            *rule_label,
        );

        // There should never be a case where a first plan is an empty production.
        // There should always be child nodes, otherwise the data structures won't work.
        let automaton = automatons.get_mut(rule_label).unwrap();
        if automaton.dfa_states[0].is_final {
            panic!(
                "The rule \"{}\" must not have an empty production",
                automaton.name
            );
        }
        automaton.dfa_states[0].transition_to_plan = match &first_plans[rule_label] {
            FirstPlan::Calculated(plans, _) => plans.clone(),
            _ => unreachable!(),
        };
    }
    // Optimize and calculate the rest of the plans
    for rule_label in &rule_labels {
        for i in 1..automatons[rule_label].dfa_states.len() {
            let (plans, _) = plans_for_dfa(
                nonterminal_map,
                &keywords,
                soft_keywords,
                &mut automatons,
                &mut first_plans,
                *rule_label,
                DFAStateId(i),
                false,
            );
            automatons.get_mut(rule_label).unwrap().dfa_states[i].transition_to_plan = plans;
        }

        // Left recursion can be calculated here, because first nodes are not relevant, because
        // they are never allowed to be final.
        for i in 1..automatons[rule_label].dfa_states.len() {
            let left_recursion_plans =
                create_left_recursion_plans(&automatons, *rule_label, DFAStateId(i), &first_plans);
            automatons.get_mut(rule_label).unwrap().dfa_states[i]
                .transition_to_plan
                .extend(left_recursion_plans);
        }
        //if nonterminal_map.get("arguments") == Some(rule_label) {
        //    println!("{}", &automatons.get(rule_label).unwrap().illustrate_dfas(nonterminal_map));
        //}
    }
    (automatons, keywords)
}

fn create_first_plans(
    nonterminal_map: &InternalStrToNode,
    keywords: &Keywords,
    soft_keywords: &SoftKeywords,
    first_plans: &mut FirstPlans,
    automatons: &mut Automatons,
    automaton_key: InternalNonterminalType,
) {
    if first_plans.get(&automaton_key).is_none() {
        first_plans.insert(automaton_key, FirstPlan::Calculating);
        let (plans, is_left_recursive) = plans_for_dfa(
            nonterminal_map,
            keywords,
            soft_keywords,
            automatons,
            first_plans,
            automaton_key,
            DFAStateId(0),
            true,
        );

        if is_left_recursive && plans.is_empty() {
            panic!(
                "The grammar contains left recursion without an \
                    alternative for rule {:?}",
                nonterminal_to_str(nonterminal_map, automaton_key)
            );
        }
        first_plans.insert(
            automaton_key,
            FirstPlan::Calculated(plans, is_left_recursive),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn plans_for_dfa(
    nonterminal_map: &InternalStrToNode,
    keywords: &Keywords,
    soft_keywords: &SoftKeywords,
    automatons: &mut Automatons,
    first_plans: &mut FirstPlans,
    automaton_key: InternalNonterminalType,
    dfa_id: DFAStateId,
    is_first_plan: bool,
) -> (SquashedTransitions, bool) {
    let mut conflict_tokens = HashSet::new();
    let mut conflict_transitions = HashSet::new();

    let mut plans = new_fast_hash_map();
    let mut is_left_recursive = false;
    // It is safe to get the dfa_state here, because they are pinned in a list that is insert only.
    let dfa_state = unsafe {
        &*(&automatons[&automaton_key].dfa_states[dfa_id.0] as &DFAState as *const DFAState)
    };
    for &transition in &dfa_state.transitions {
        match transition.type_ {
            TransitionType::Terminal(type_, debug_text) => {
                let t = type_.to_squashed();
                add_if_no_conflict(
                    &mut plans,
                    &mut conflict_transitions,
                    &mut conflict_tokens,
                    transition,
                    t,
                    || Plan {
                        pushes: Vec::new(),
                        next_dfa: transition.to,
                        type_: t,
                        debug_text,
                        is_left_recursive: false,
                    },
                );
                if let Some(kws) = soft_keywords.get(&type_) {
                    for &kw in kws {
                        let soft_keyword_type = keywords.get_squashed(kw).unwrap();
                        add_if_no_conflict(
                            &mut plans,
                            &mut conflict_transitions,
                            &mut conflict_tokens,
                            DFATransition {
                                type_: TransitionType::Keyword(kw),
                                to: transition.to,
                            },
                            soft_keyword_type,
                            || Plan {
                                pushes: Vec::new(),
                                next_dfa: transition.to,
                                type_: t,
                                debug_text,
                                is_left_recursive: false,
                            },
                        );
                    }
                }
            }
            TransitionType::Nonterminal(node_id) => {
                if is_first_plan {
                    if let Some(FirstPlan::Calculating) = first_plans.get(&node_id) {
                        if node_id != automaton_key {
                            panic!(
                                "Indirect left recursion not supported (in rule {:?})",
                                nonterminal_to_str(nonterminal_map, automaton_key)
                            );
                        }
                        is_left_recursive = true;
                        continue;
                    }
                    create_first_plans(
                        nonterminal_map,
                        keywords,
                        soft_keywords,
                        first_plans,
                        automatons,
                        node_id,
                    );
                }
                match &first_plans[&node_id] {
                    FirstPlan::Calculated(transitions, is_left_recursive) => {
                        for (&t, nested_plan) in transitions {
                            add_if_no_conflict(
                                &mut plans,
                                &mut conflict_transitions,
                                &mut conflict_tokens,
                                transition,
                                t,
                                || {
                                    nest_plan(
                                        nested_plan,
                                        node_id,
                                        transition.to,
                                        StackMode::Normal,
                                    )
                                },
                            );
                        }
                    }
                    FirstPlan::Calculating => {
                        unreachable!()
                    }
                }
            }
            TransitionType::Keyword(keyword) => {
                let t = keywords.get_squashed(keyword).unwrap();
                add_if_no_conflict(
                    &mut plans,
                    &mut conflict_transitions,
                    &mut conflict_tokens,
                    transition,
                    t,
                    || Plan {
                        pushes: Vec::new(),
                        next_dfa: transition.to,
                        type_: t,
                        debug_text: keyword,
                        is_left_recursive: false,
                    },
                );
            }
            TransitionType::PositiveLookaheadStart => {
                let (inner_plans, inner_is_left_recursive) = plans_for_dfa(
                    nonterminal_map,
                    keywords,
                    soft_keywords,
                    automatons,
                    first_plans,
                    automaton_key,
                    transition.get_next_dfa().list_index,
                    is_first_plan,
                );
                if inner_is_left_recursive {
                    panic!(
                        "Left recursion with lookaheads is not supported (in rule {:?})",
                        nonterminal_to_str(nonterminal_map, automaton_key)
                    );
                }
                for (&t, plan) in &create_lookahead_plans(automaton_key, transition, &inner_plans) {
                    add_if_no_conflict(
                        &mut plans,
                        &mut conflict_transitions,
                        &mut conflict_tokens,
                        transition,
                        t,
                        || plan.clone(),
                    );
                }
            }
            TransitionType::NegativeLookaheadStart => {
                let dfa = transition.get_next_dfa();
                let lookahead_end = dfa.transitions[0].get_next_dfa();
                assert!(lookahead_end.is_lookahead_end());
                assert_eq!(lookahead_end.transitions.len(), 1);

                let next_dfa = lookahead_end.transitions[0].get_next_dfa();
                // Only simple peeks are allowed at the moment.
                let (mut inner_plans, _) = plans_for_dfa(
                    nonterminal_map,
                    keywords,
                    soft_keywords,
                    automatons,
                    first_plans,
                    automaton_key,
                    next_dfa.list_index,
                    is_first_plan,
                );
                for transition in &dfa.transitions {
                    let t = match transition.type_ {
                        TransitionType::Terminal(type_, debug_text) => type_.to_squashed(),
                        TransitionType::Keyword(keyword) => keywords.get_squashed(keyword).unwrap(),
                        _ => {
                            panic!("Only terminal lookaheads are allowed");
                        }
                    };
                    // Negative lookaheads are only allowed to be simple terminals.
                    // However we can not just remove those terminals from plans,
                    // because that would not be sufficient for final states.
                    let automaton = &automatons[&automaton_key];
                    let empty_dfa_id = automaton.no_transition_dfa_id.unwrap();
                    inner_plans.insert(t, Plan {
                        debug_text: "negative lookahead abort",
                        is_left_recursive: false,
                        next_dfa: &*automaton.dfa_states[empty_dfa_id.0],
                        pushes: vec!(),
                        type_: t,
                    });
                }
                plans.extend(
                    inner_plans
                        .iter()
                        .map(|(&key, plan)| (key, (transition, plan.clone()))),
                );
            }
            TransitionType::LookaheadEnd => {
                // No plans need to be created.
                continue;
            }
        }
    }

    /*
    if conflict_transitions.len() > 0 {
        dbg!(&conflict_transitions, &automatons[&automaton_key].name);
        for x in &conflict_transitions {
            if let TransitionType::Nonterminal(id) = x {
                dbg!(nonterminal_to_str(nonterminal_map, *id));
            }
        }
    }
    */
    for c in &conflict_tokens {
        debug_assert!(!plans.contains_key(c));
    }

    let mut result: SquashedTransitions = plans
        .iter()
        .map(|(&t, (_, plan))| (t, plan.clone()))
        .collect();
    if !conflict_tokens.is_empty() {
        let automaton = automatons.get_mut(&automaton_key).unwrap();
        let (start, end) = split_tokens(automaton, &dfa_state, conflict_transitions);
        let t = automaton.type_;
        for dfa_id in (start..automaton.dfa_states.len()).rev() {
            let (new_plans, left_recursive) = plans_for_dfa(
                nonterminal_map,
                keywords,
                soft_keywords,
                automatons,
                first_plans,
                automaton_key,
                DFAStateId(dfa_id),
                is_first_plan,
            );
            debug_assert!(!left_recursive);
            for (transition, mut new_plan) in new_plans {
                if conflict_tokens.contains(&transition) {
                    if let Some(fallback_plan) = result.remove(&transition) {
                        let automaton = automatons.get_mut(&automaton_key).unwrap();
                        // This sets a const pointer on the fallback plan. This is only save,
                        // because the plans are not touched after they have been generated.
                        automaton
                            .fallback_plans
                            .push(Pin::new(Box::new(fallback_plan)));
                        new_plan = nest_plan(
                            &new_plan,
                            t,
                            end,
                            StackMode::Alternative(
                                automaton.fallback_plans.last().unwrap() as &Plan
                            ),
                        );
                    }
                    //dbg!(&transition, &new_plan);
                    result.insert(transition, new_plan);
                }
            }
        }
    }
    (result, is_left_recursive)
}

fn add_if_no_conflict<F: FnOnce() -> Plan>(
    plans: &mut FastHashMap<InternalSquashedType, (DFATransition, Plan)>,
    conflict_transitions: &mut HashSet<TransitionType>,
    conflict_tokens: &mut HashSet<InternalSquashedType>,
    transition: DFATransition,
    token: InternalSquashedType,
    create_plan: F,
) {
    if conflict_tokens.contains(&token) {
        conflict_transitions.insert(transition.type_);
    } else {
        if let Some(&(t_x, _)) = plans.get(&token) {
            if t_x.type_ != transition.type_ {
                plans.remove(&token);
                conflict_tokens.insert(token);
                conflict_transitions.insert(transition.type_);
                conflict_transitions.insert(t_x.type_);
                return;
            }
        }
        plans.insert(token, (transition, create_plan()));
    }
}

fn create_lookahead_plans(
    automaton_key: InternalNonterminalType,
    transition: DFATransition,
    inner_plans: &SquashedTransitions,
) -> SquashedTransitions {
    let mode = match transition.type_ {
        TransitionType::PositiveLookaheadStart => StackMode::PositiveLookahead,
        _ => unreachable!(),
    };
    inner_plans
        .iter()
        .map(|(k, plan)| {
            (
                *k,
                nest_plan(
                    plan,
                    automaton_key,
                    search_lookahead_end(plan.get_next_dfa()),
                    mode,
                ),
            )
        })
        .collect()
}

fn create_left_recursion_plans(
    automatons: &Automatons,
    automaton_key: InternalNonterminalType,
    dfa_id: DFAStateId,
    first_plans: &FirstPlans,
) -> SquashedTransitions {
    let mut plans = new_fast_hash_map();
    let automaton = &automatons[&automaton_key];
    let dfa_state = &automaton.dfa_states[dfa_id.0];
    if dfa_state.is_final && !dfa_state.is_lookahead_end() {
        // DFAStates that are the end of a lookahead are ignored here, because left recursion is
        // not allowed for lookaheads and they get a separate stack node anyway.
        match first_plans[&automaton.type_] {
            FirstPlan::Calculated(_, is_left_recursive) => {
                if is_left_recursive {
                    for transition in &automaton.dfa_states[0].transitions {
                        if let TransitionType::Nonterminal(node_id) = transition.type_ {
                            if node_id == automaton.type_ {
                                for (&t, p) in &transition.get_next_dfa().transition_to_plan {
                                    if plans.contains_key(&t) {
                                        panic!("ambigous: {} contains left recursion with alternatives!",
                                               dfa_state.from_rule);
                                    }
                                    plans.insert(
                                        t,
                                        Plan {
                                            pushes: p.pushes.clone(),
                                            next_dfa: p.next_dfa,
                                            type_: t,
                                            debug_text: p.debug_text,
                                            is_left_recursive: true,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    plans
}

fn nest_plan(
    plan: &Plan,
    new_node_id: InternalNonterminalType,
    next_dfa: *const DFAState,
    mode: StackMode,
) -> Plan {
    let mut pushes = plan.pushes.clone();
    pushes.insert(
        0,
        Push {
            node_type: new_node_id,
            next_dfa: plan.next_dfa,
            stack_mode: mode,
        },
    );
    Plan {
        pushes,
        next_dfa,
        type_: plan.type_,
        debug_text: plan.debug_text,
        is_left_recursive: false,
    }
}

fn search_lookahead_end(dfa_state: &DFAState) -> *const DFAState {
    let mut already_checked = HashSet::new();
    already_checked.insert(dfa_state.list_index);

    fn search(already_checked: &mut HashSet<DFAStateId>, dfa_state: &DFAState) -> *const DFAState {
        for transition in &dfa_state.transitions {
            match transition.type_ {
                TransitionType::LookaheadEnd => return transition.to,
                TransitionType::PositiveLookaheadStart | TransitionType::NegativeLookaheadStart => {
                    unimplemented!()
                }
                _ => {
                    let to_dfa = transition.get_next_dfa();
                    if !already_checked.contains(&to_dfa.list_index) {
                        already_checked.insert(to_dfa.list_index);
                        // It is a bit weird that this return works. It probably works, but maybe
                        // wouldn't in some weird cases. We can still fix it then if necessary.
                        return search(already_checked, to_dfa);
                    }
                }
            }
        }
        unreachable!()
    }
    search(&mut already_checked, dfa_state)
}

fn split_tokens(
    automaton: &mut RuleAutomaton,
    dfa: &DFAState,
    conflict_transitions: HashSet<TransitionType>,
) -> (usize, *const DFAState) {
    let mut transition_to_nfas = new_fast_hash_map::<_, Vec<_>>();
    let mut nfas: Vec<_> = dfa.nfa_set.iter().collect();
    nfas.sort_by_key(|id| id.0);
    for &nfa_id in &nfas {
        let nfa = &automaton.nfa_states[nfa_id.0];
        for transition in &nfa.transitions {
            if let Some(t) = transition.type_ {
                if conflict_transitions.contains(&t) {
                    if let Some(list) = transition_to_nfas.get_mut(&t) {
                        list.push(nfa_id);
                    } else {
                        transition_to_nfas.insert(t, vec![nfa_id]);
                    }
                }
            }
        }
    }

    let end_dfa = automaton.nfa_to_dfa(vec![automaton.nfa_end_id], automaton.nfa_end_id);
    let first_new_index = automaton.dfa_states.len();

    let mut as_list: Vec<_> = transition_to_nfas
        .iter()
        .map(|(_, nfa_ids)| nfa_ids.clone())
        .collect();
    while !as_list.is_empty() {
        as_list.sort_by_key(|nfa_ids| nfa_ids[0].0);
        let mut new_dfa_nfa_ids = vec![];
        if as_list.len() > 1 {
            let must_be_smaller = *as_list[1][0];
            debug_assert!(!as_list[0].is_empty());
            while let Some(&&nfa_id) = as_list[0].get(0) {
                // It should basically never happen that two transitions are possible from an
                // NFA. ε-moves are of course always possible.
                debug_assert!(nfa_id != must_be_smaller);
                if nfa_id.0 > must_be_smaller.0 {
                    break;
                }
                new_dfa_nfa_ids.push(*as_list[0].remove(0));
            }
            if as_list[0].is_empty() {
                as_list.remove(0);
            }
        } else {
            new_dfa_nfa_ids.extend(as_list.pop().unwrap().iter().cloned());
        }
        debug_assert!(!new_dfa_nfa_ids.is_empty());

        let dfa = automaton.nfa_to_dfa(new_dfa_nfa_ids, automaton.nfa_end_id);
        automaton.construct_powerset_for_dfa(dfa, automaton.nfa_end_id);
        //dbg!(x.dfa_states.len(), x.dfa_states.last().unwrap());
    }
    (first_new_index, end_dfa)
}

fn nonterminal_to_str(
    nonterminal_map: &InternalStrToNode,
    nonterminal: InternalNonterminalType,
) -> &str {
    for (k, v) in nonterminal_map {
        if nonterminal == *v {
            return *k;
        }
    }
    panic!("Something is very wrong, integer not found");
}
