use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::fmt::{Debug, Formatter, Result as FmtResult};
use std::sync::{Arc};
use std::iter;
use std::iter::FromIterator;
// use std::io::{Error};

use rayon::prelude::*;
use proptest::prelude::*;

use proptest::test_runner::Config;
use proptest::test_runner::TestRunner;
use proptest::strategy::ValueTree;
use rand::{thread_rng, Rng};

use traits::{Estimate, Zero, Sender, Data};
use justification::{Justification, SenderState, LatestMsgs, LatestMsgsHonest};
use weight_unit::{WeightUnit};
use senders_weight::SendersWeight;

pub trait CasperMsg: Hash + Ord + Clone + Eq + Sync + Send + Debug {
    // To be implemented on concrete struct
    type Sender: Sender;
    type Estimate: Estimate<M = Self>;
    fn get_sender(&self) -> &Self::Sender;
    fn get_estimate(&self) -> &Self::Estimate;
    fn get_justification<'z>(&'z self) -> &'z Justification<Self>;
    fn new(
        sender: Self::Sender,
        justification: Justification<Self>,
        estimate: Self::Estimate,
    ) -> Self;
    // this function is used to clean up memory. when a msg is final, there's no
    // need to keep its justifications. when dropping its justification, all the
    // Msgs (Arc) which are referenced on the justification will get dropped
    // from memory
    fn set_as_final(&mut self);

    // Following methods are actual implementations

    /// create a msg from newly received messages
    fn from_msgs(
        sender: Self::Sender,
        new_msgs: Vec<&Self>,
        finalized_msg: Option<&Self>,
        sender_state: &SenderState<Self>,
        external_data: Option<<<Self as CasperMsg>::Estimate as Data>::Data>,
    ) -> Result<(Self, SenderState<Self>), &'static str> {
        // // TODO eventually comment out these lines, and FIXME tests
        // // check whether two messages from same sender
        // let mut senders = HashSet::new();
        // let dup_senders = new_msgs.iter().any(|msg| !senders.insert(msg.get_sender()));
        // assert!(!dup_senders, "A sender can only have one, and only one, latest message");

        // dedup by putting msgs into a hashset
        let new_msgs: HashSet<_> = new_msgs.iter().cloned().collect();

        let mut justification = Justification::new();
        sender_state.get_my_last_msg().as_ref().map_or_else(
            // || println!("No commitment to any previous state!"),
            || (),
            |my_last_msg| {
                justification.insert(my_last_msg.clone());
            },
        );
        let (success, sender_state) =
            justification.faulty_inserts(new_msgs, &sender_state);
        if !success {
            Err("None of the messages could be added to the state!")
        } else {
            let latest_msgs_honest = LatestMsgsHonest::from_latest_msgs(
                sender_state.get_latest_msgs(),
                sender_state.get_equivocators(),
            );
            let estimate = latest_msgs_honest.mk_estimate(
                finalized_msg,
                &sender_state.get_senders_weights(),
                external_data,
            );
            let message = Self::new(sender, justification, estimate);
            Ok((message, sender_state))
        }
    }

    /// insanely expensive
    fn equivocates_indirect(
        &self,
        rhs: &Self,
        mut equivocators: HashSet<<Self as CasperMsg>::Sender>,
    ) -> (bool, HashSet<<Self as CasperMsg>::Sender>) {
        let is_equivocation = self.equivocates(rhs);
        let init = if is_equivocation {
            equivocators.insert(self.get_sender().clone());
            (true, equivocators)
        } else {
            (false, equivocators)
        };
        self.get_justification().iter().fold(
            init,
            |(acc_has_equivocations, acc_equivocators), self_prime| {
                // note the rotation between rhs and self, done because
                // descending only on self, thus rhs has to become self on the
                // recursion to get its justification visited
                let (has_equivocation, equivocators) = rhs
                    .equivocates_indirect(self_prime, acc_equivocators.clone());
                let acc_equivocators =
                    acc_equivocators.union(&equivocators).cloned().collect();
                (acc_has_equivocations || has_equivocation, acc_equivocators)
            },
        )
    }
    fn equivocates(&self, rhs: &Self) -> bool {
        self != rhs
            && self.get_sender() == rhs.get_sender()
            && !rhs.depends(self)
            && !self.depends(rhs)
    }
    fn depends(&self, rhs: &Self) -> bool {
        // TODO: this can be done with inner recursion function to keep track of
        // what was visited

        // although the recursion ends supposedly only at genesis message, the
        // trick is the following: it short-circuits while descending on the
        // dependency tree, if it finds a dependent message. when dealing with
        // honest validators, this would return true very fast. all the new
        // derived branches of the justification will be evaluated in parallel.
        // say if a msg is justified by 2 other msgs, then the 2 other msgs will
        // be processed on different threads. this applies recursively, so if
        // each of the 2 msgs have say 3 justifications, then each of the 2
        // threads will spawn 3 new threads to process each of the messages.
        // thus, highly parallelizable. when it shortcuts, because in one thread
        // a dependency was found, the function returns true and all the
        // computation on the other threads will be canceled.
        let justification = self.get_justification();
        justification.contains(rhs)
            || justification
                .par_iter()
                .any(|self_prime| self_prime.depends(rhs))
    }
    fn is_circular(&self, rhs: &Self) -> bool {
        rhs.depends(self) && self.depends(rhs)
    }

    /// returns the dag tip-most safe messages. a safe message is defined as one
    /// that was seen by all senders (with non-zero weight in senders_weights)
    /// and all senders saw each other seeing each other messages.
    fn get_msg_for_proposition(
        &self,
        all_senders: &HashSet<Self::Sender>,
    ) -> HashSet<Self> {
        fn recursor<M>(
            m: &M,
            all_senders: &HashSet<M::Sender>,
            mut senders_referred: HashSet<M::Sender>,
            safe_ms: HashSet<M>,
            original_sender: &M::Sender,
        ) -> HashSet<M>
        where
            M: CasperMsg,
        {
            m.get_justification().iter().fold(
                safe_ms,
                |mut safe_ms_prime, m_prime| {
                    // base case
                    if &senders_referred == all_senders
                        && original_sender == m_prime.get_sender()
                    {
                        let _ = safe_ms_prime.insert(m_prime.clone());
                        safe_ms_prime
                    } else {
                        let _ = senders_referred
                            .insert(m_prime.get_sender().clone());
                        recursor(
                            m_prime,
                            all_senders,
                            senders_referred.clone(),
                            safe_ms_prime,
                            original_sender,
                        )
                    }
                },
            )
        };
        // initial state, trigger recursion
        let original_sender = self.get_sender();
        let senders_refered =
            [original_sender.clone()].iter().cloned().collect();
        let safe_msgs = HashSet::new();
        recursor(
            self,
            all_senders,
            senders_refered,
            safe_msgs,
            &original_sender,
        )
    }

    // fn get_msg_for_prop2(
    //     &self,
    //     all_senders: &HashSet<Self::Sender>,
    //     equivocators: : &HashSet<Self::Sender>,

    // )

    /// returns the dag tip-most safe messages. a safe message is defined as one
    /// that was seen by more than a given thr of total weight units. TODO: not
    /// sure about this implementation, i assume its not working correctly.
    fn get_safe_msgs_by_weight(
        &self,
        senders_weights: &SendersWeight<Self::Sender>,
        thr: &WeightUnit,
    ) -> HashMap<Self, WeightUnit> {
        fn recursor<M>(
            m: &M,
            senders_weights: &SendersWeight<M::Sender>,
            mut senders_referred: HashSet<M::Sender>,
            weight_referred: WeightUnit,
            thr: &WeightUnit,
            safe_msg_weight: HashMap<M, WeightUnit>,
        ) -> HashMap<M, WeightUnit>
        where
            M: CasperMsg,
        {
            m.get_justification().iter().fold(
                safe_msg_weight,
                |mut safe_msg_weight_prime, m_prime| {
                    // base case
                    if &weight_referred > thr {
                        let _ = safe_msg_weight_prime
                            .insert(m.clone(), weight_referred);
                        safe_msg_weight_prime
                    } else {
                        let sender_current = m_prime.get_sender();
                        let weight_referred = if senders_referred
                            .insert(sender_current.clone())
                        {
                            weight_referred
                                + senders_weights
                                    .get_weight(&sender_current)
                                    .unwrap_or(WeightUnit::ZERO)
                        } else {
                            weight_referred
                        };

                        recursor(
                            m_prime,
                            senders_weights,
                            senders_referred.clone(),
                            weight_referred,
                            thr,
                            safe_msg_weight_prime,
                        )
                    }
                },
            )
        };
        // initial state, trigger recursion
        let senders_referred = [].iter().cloned().collect();
        let weight_referred = WeightUnit::ZERO;
        let safe_msg_weight = HashMap::new();
        recursor(
            self,
            senders_weights,
            senders_referred,
            weight_referred,
            thr,
            safe_msg_weight,
        )
    }
}

#[derive(Clone, Default, Eq, PartialEq, PartialOrd, Ord)]
struct ProtoMsg<E, S>
where
    E: Estimate<M = Message<E, S>>,
    S: Sender,
{
    estimate: E,
    sender: S,
    justification: Justification<Message<E, S>>,
}

#[derive(Eq, Ord, PartialOrd, Clone, Default)]
pub struct Message<E, S>(Box<Arc<ProtoMsg<E, S>>>)
where
    E: Estimate<M = Message<E, S>>,
    S: Sender;

/*
// TODO start we should make messages lazy. continue this after async-await is better
// documented

enum MsgStatus {
Unchecked,
Valid,
Invalid,
}

struct Message<E, S, D>
where
    E: Estimate,
    S: Sender,
{
    status: MsgStatus,
    msg: Future<Item = Message<E, S, D>, Error = Error>,
}
// TODO end
*/

impl<E, S> From<ProtoMsg<E, S>> for Message<E, S>
where
    E: Estimate<M = Self>,
    S: Sender,
{
    fn from(msg: ProtoMsg<E, S>) -> Self {
        Message(Box::new(Arc::new(msg)))
    }
}

impl<E, S> CasperMsg for Message<E, S>
where
    E: Estimate<M = Self>,
    S: Sender,
{
    type Estimate = E;
    type Sender = S;

    fn get_sender(&self) -> &Self::Sender {
        &self.0.sender
    }
    fn get_estimate(&self) -> &Self::Estimate {
        &self.0.estimate
    }
    fn get_justification<'z>(&'z self) -> &'z Justification<Self> {
        &self.0.justification
    }

    fn new(sender: S, justification: Justification<Self>, estimate: E) -> Self {
        Message::from(ProtoMsg {
            sender,
            justification,
            estimate,
        })
    }

    fn set_as_final(&mut self) {
        let mut proto_msg = (**self.0).clone();
        proto_msg.justification = Justification::new();
        *self.0 = Arc::new(proto_msg);
    }
}

impl<E, S> Hash for Message<E, S>
where
    E: Estimate<M = Self>,
    S: Sender,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        let _ = self.get_sender().hash(state);
        let _ = self.get_justification().hash(state);
        let _ = self.get_estimate().hash(state); // the hash of the msg does depend on the estimate
    }
}

impl<E, S> PartialEq for Message<E, S>
where
    E: Estimate<M = Self>,
    S: Sender,
{
    fn eq(&self, rhs: &Self) -> bool {
        self.get_sender() == rhs.get_sender()
            && self.get_justification() == rhs.get_justification()
            && self.get_estimate() == rhs.get_estimate()
    }
}

impl<E, S> Debug for Message<E, S>
where
    E: Estimate<M = Self>,
    S: Sender,
{
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        write!(
            f,
            "M{:?}({:?})",
            // "M{:?}({:?}) -> {:?}",
            self.get_sender(),
            self.get_estimate().clone(),
            // self.get_justification()
        )
    }
}

#[cfg(test)]
mod message {
    use example::vote_count::{VoteCount};
    use senders_weight::{SendersWeight};
    use example::blockchain::*;

    use std::{f64};
    use super::*;

    fn add_message<'z, M>(
        state: &'z mut BTreeMap<M::Sender, SenderState<M>>,
        sender: M::Sender,
        recipients: HashSet<M::Sender>,
    ) -> &'z BTreeMap<M::Sender, SenderState<M>>
    where
        M: CasperMsg
    {
        // println!("{:?} {:?}", sender, recipients);
        let latest_honest_msgs = LatestMsgsHonest::from_latest_msgs(
            &state[&sender].get_latest_msgs(),
            &HashSet::new(),
        );
        let (justification, sender_state) = Justification::from_msgs(
            latest_honest_msgs.iter().cloned().collect(),
            &state[&sender],
        );
        // let latest_honest_msgs = LatestMsgsHonest::from_latest_msgs(
        //     &state[&sender].get_latest_msgs(),
        //     &HashSet::new(),
        // );
        // let ghost = Block::ghost(&latest_honest_msgs, None, state[&sender].get_senders_weights());
        let estimate = latest_honest_msgs.mk_estimate(
            None,
            state[&sender].get_senders_weights(),
            None,
        );
        // println!("Sender: {:?}", sender);
        // println!("Ghost calc: {:?}", ghost);
        // println!("Justification: {:?}", justification);
        let m = M::new(sender.clone(), justification, estimate);
        // println!("Message added: {:?}\n", m);
        let (_, sender_state) = Justification::from_msgs(
            LatestMsgsHonest::from_latest_msgs(
                sender_state.get_latest_msgs(),
                &HashSet::new(),
            ).iter()
                .cloned()
                .collect(),
            &sender_state,
        );
        state.insert(sender, sender_state);
        recipients.iter().for_each(|recipient| {
            let (_, recipient_state) =
                Justification::from_msgs(vec![m.clone()], &state[recipient]);
            state.insert(recipient.clone(), recipient_state);
        });
        // println!("Message added: {:?}\n", m);
        state
    }

    fn round_robin(val: &mut Vec<u32>) -> BoxedStrategy<u32> {
        let v = val.pop().unwrap();
        val.insert(0, v);
        Just(v).boxed()
    }

    fn arbitrary_in_set(val: &mut Vec<u32>) -> BoxedStrategy<u32> {
        prop::sample::select(val.clone()).boxed()
    }

    fn all_receivers(val: &Vec<u32>) -> BoxedStrategy<HashSet<u32>> {
        let mut v = HashSet::from_iter(val.iter().cloned());
        // v.remove(&0);
        Just(v).boxed()
    }

    fn some_receivers(val: &Vec<u32>) -> BoxedStrategy<HashSet<u32>> {
        // let x = vec![0,1];
        // let res = prop::collection::hash_set(
        //     prop::sample::select(val.clone()),
        //     1..(val.len()),
        // ).boxed();
        prop::collection::hash_set(
            prop::sample::select(val.clone()),
            0..(val.len()),
        ).boxed()
    }

    // fn some_deceivers(val: &'static Vec<u32>) -> BoxedStrategy<HashSet<u32>> {
    fn some_deceivers(val: &Vec<u32>) -> BoxedStrategy<HashSet<u32>> {
        // let mut v = HashSet::from_iter(val.iter().cloned());
        // let validators: Vec<usize> = (1..val.len()).collect();
        // prop::sample::select(validators).prop_flat_map(move |size|
        //     prop::sample::subsequence(val.clone(), size)
        //         .prop_map(|subsequence| {
        //             HashSet::from_iter(subsequence.iter().cloned())
        //         }))
        //     .boxed()
        let mut rng = thread_rng();
        let gen = rng.gen_range(0, val.len());

        // println!("Input: {:?}", val);
        // println!("Result: {:?}", gen);
        // let validators: Vec<usize> = (1..val.len()).collect();
        let res = prop::sample::subsequence(val.clone(), gen)
            .prop_map(|subsequence| {
                HashSet::from_iter(subsequence.iter().cloned())
            })
            .boxed();
        res
    }

    fn message_event(
        state: BTreeMap<u32, SenderState<BlockMsg>>,
        sender_strategy: BoxedStrategy<u32>,
        receiver_strategy: BoxedStrategy<HashSet<u32>>,
    ) -> BoxedStrategy<BTreeMap<u32, SenderState<BlockMsg>>> {
        (sender_strategy, receiver_strategy, Just(state))
            .prop_map(|(sender, mut receivers, mut state)| {
                // let receivers = state.keys().cloned().collect();
                // println!("receivers: {:?}", receivers);
                if !receivers.contains(&sender) {receivers.insert(sender);}
                add_message(&mut state, sender, receivers).clone()
            })
            .boxed()
    }

    fn full_consensus(state: BTreeMap<u32, SenderState<BlockMsg>>) -> bool {
        let m: HashSet<_> = state
            .iter()
            .map(|(_, sender_state)| {
                let latest_honest_msgs = LatestMsgsHonest::from_latest_msgs(
                    sender_state.get_latest_msgs(),
                    &HashSet::new(),
                );
                latest_honest_msgs.mk_estimate(
                    None,
                    sender_state.get_senders_weights(),
                    None,
                )
            })
            .collect();
        println!("{:?}", m);
        m.len() == 1
    }

    fn safety_oracle(state: BTreeMap<u32, SenderState<BlockMsg>>) -> bool {
        let m: HashSet<_> = state
            .iter()
            .map(|(_, sender_state)| {
                let latest_honest_msgs = LatestMsgsHonest::from_latest_msgs(
                    sender_state.get_latest_msgs(),
                    &HashSet::new(),
                );
                let genesis_block = Block::from(ProtoBlock {
                    prevblock: None,
                    sender: 0,
                    txs: BTreeSet::new(),
                });
                // println!(
                //     "Cliques: {:?}\n",
                //     Block::safety_oracles(
                //         genesis_block.clone(),
                //         &latest_honest_msgs,
                //         &HashSet::new(),
                //         0.0,
                //         sender_state.get_senders_weights()
                //     )
                // );
                let safety_threshold =
                    (sender_state.get_senders_weights().sum_all_weights())
                        / 2.0;
                // println!(
                //     "Safety oracles: {:?}\n",
                //     Block::safety_oracles(
                //         genesis_block.clone(),
                //         &latest_honest_msgs,
                //         &HashSet::new(),
                //         safety_threshold,
                //         sender_state.get_senders_weights()
                //     )
                // );
                Block::safety_oracles(
                    genesis_block,
                    &latest_honest_msgs,
                    &HashSet::new(),
                    safety_threshold,
                    sender_state.get_senders_weights(),
                ) != BTreeSet::new()
            })
            .collect();
        m.contains(&true)
    }

    fn safety_oracle_verbatim(state: BTreeMap<u32, SenderState<BlockMsg>>) -> Vec<Vec<Vec<u32>>> {
        // println!("here");
        let m: Vec<_> = state
            .iter()
            .map(|(sender, sender_state)| {
                let latest_honest_msgs = LatestMsgsHonest::from_latest_msgs(
                    sender_state.get_latest_msgs(),
                    &HashSet::new(),
                );
                let genesis_block = Block::from(ProtoBlock {
                    prevblock: None,
                    sender: 0,
                    txs: BTreeSet::new(),
                });
                // println!(
                //     "Clikues: {:?}\n",
                //     Block::safety_oracles(
                //         genesis_block.clone(),
                //         &latest_honest_msgs,
                //         &HashSet::new(),
                //         0.0,
                //         sender_state.get_senders_weights()
                //     )
                // );
                let safety_threshold =
                    (sender_state.get_senders_weights().sum_all_weights())
                        / 2.0;
                // println!(
                //     "Safety oracles: {:?}\n",
                //     Block::safety_oracles(
                //         genesis_block.clone(),
                //         &latest_honest_msgs,
                //         &HashSet::new(),
                //         safety_threshold,
                //         sender_state.get_senders_weights()
                //     )
                // );
                let a = Block::safety_oracles(
                    genesis_block,
                    &latest_honest_msgs,
                    &HashSet::new(),
                    0.0,
                    sender_state.get_senders_weights(),
                );
                let c = a.clone();
                let b: Vec<BTreeSet<u32>> = Vec::from_iter(c.iter().cloned());
                let d: Vec<Vec<u32>> = b.iter().map(|btree| Vec::from_iter(btree.iter().cloned())).collect();
                d
            })
            .collect();
        m
    }

    fn chain<F: 'static, G: 'static, H: 'static>(
        consensus_value_strategy: BoxedStrategy<Block>,
        validator_max_count: usize,
        message_producer_strategy: F,
        message_receiver_strategy: G,
        consensus_satisfied: H,
    ) -> BoxedStrategy<Vec<BTreeMap<u32, SenderState<BlockMsg>>>>
    where
        F: Fn(&mut Vec<u32>) -> BoxedStrategy<u32>,
        G: Fn(&Vec<u32>) -> BoxedStrategy<HashSet<u32>>,
        H: Fn(BTreeMap<u32, SenderState<BlockMsg>>) -> bool,
    {
        (prop::sample::select((1..validator_max_count).collect::<Vec<usize>>()))
            .prop_flat_map(move |validators| {
                (prop::collection::vec(
                    consensus_value_strategy.clone(),
                    validators,
                ))
            })
            .prop_map(move |votes| {
                let mut state = BTreeMap::new();
                // println!("{:?}: {:?}", votes.len(), votes);
                let validators: Vec<u32> = (0..votes.len() as u32).collect();

                let weights: Vec<f64> =
                    iter::repeat(1.0).take(votes.len() as usize).collect();

                let senders_weights = SendersWeight::new(
                    validators
                        .iter()
                        .cloned()
                        .zip(weights.iter().cloned())
                        .collect(),
                );

                validators.iter().for_each(|validator| {
                    let mut j = Justification::new();
                    let m = Message::new(
                        *validator,
                        j.clone(),
                        votes[*validator as usize].clone(),
                    );
                    j.insert(m.clone());
                    state.insert(
                        *validator,
                        SenderState::new(
                            senders_weights.clone(),
                            0.0,
                            Some(m),
                            LatestMsgs::from(&j),
                            0.0,
                            HashSet::new(),
                        ),
                    );
                });

                let mut runner = TestRunner::default();
                // let mut senders: Vec<_> = state.keys().cloned().collect();
                let mut senders = validators.clone();
                // println!("k");
                let chain = iter::repeat_with(|| {
                    // println!("a");
                    let sender_strategy =
                        message_producer_strategy(&mut senders);
                    let receiver_strategy = message_receiver_strategy(&senders);
                    state = message_event(
                        state.clone(),
                        sender_strategy,
                        receiver_strategy,
                    ).new_value(&mut runner)
                        .unwrap()
                        .current();
                    // println!("b");
                    let k = state.clone();
                    // println!("b'");
                    k
                });
                let mut have_consensus = false;
                let mut cont = true;
                Vec::from_iter(chain.take_while(|state| {
                    // count += 1;
                    // count < 4 && !consensus_satisfied(state.clone())
                    if have_consensus {cont = false}
                    // let x = state.clone();
                    // println!("{:?}", x);
                    if consensus_satisfied(state.clone()) {have_consensus = true}
                    // println!("d");
                    cont
                }))
            })
            .boxed()
    }

    fn arbitrary_blockchain() -> BoxedStrategy<Block> {
        let genesis_block = Block::from(ProtoBlock {
            prevblock: None,
            sender: 0,
            txs: BTreeSet::new(),
        });
        Just(genesis_block).boxed()
    }

    proptest! {
        #![proptest_config(Config::with_cases(100))]
        #[test]
        fn round_robin_blockchain(ref chain in chain(arbitrary_blockchain(), 6, arbitrary_in_set, all_receivers, safety_oracle)) {
        // fn round_robin_blockchain(ref chain in chain(arbitrary_blockchain(), 9, round_robin, all_receivers, safety_oracle)) {
            // total messages until unilateral consensus
            println!("new chain");
            chain.iter().for_each(|state| {println!("{{lms: {:?},", state.iter().map(|(_, sender_state)|
                                                                             sender_state.get_latest_msgs()).collect::<Vec<_>>());
                                           println!("sendercount: {:?},", state.keys().len());
                                           print!("clqs: ");
                                           // safety_oracle_verbatim(state.clone()).iter().for_each(|clq| print!("{:?},", clq));
                                           println!("{:?}a}}", safety_oracle_verbatim(state.clone()));
                                           // println!("clqs: quwel{:?}quwela}}", safety_oracle_verbatim(state.clone()))
            });
            // println!("{} validators -> {:?} message(s)",
            //          match chain.last().unwrap_or(&BTreeMap::new()).keys().len().to_string().as_ref()
            //          {"0" => "Unknown",
            //           a => a},
            //          chain.len() + 1);
            // assert!(chain.last().unwrap_or(&BTreeMap::new()).keys().len() >=
            //         chain.len(),
            //         "round robin with n validators should converge in at most n messages")
        }
    }

    // proptest! {
    //     #![proptest_config(Config::with_cases(30))]
    //     #[test]
    //     fn round_robin_vote_count(ref chain in chain(VoteCount::arbitrary(), 15, round_robin, all_receivers, full_consensus)) {
    //         assert_eq!(chain.last().unwrap_or(&BTreeMap::new()).keys().len(),
    //                    if chain.len() > 0 {chain.len() + 1} else {0},
    //                    "round robin with n validators should converge in n messages")
    //     }
    // }

    // proptest! {
    //     #![proptest_config(Config::with_cases(30))]
    //     #[test]
    //     fn round_robin_binary(ref chain in chain(prop::bool::ANY.boxed(), 15, round_robin, all_receivers, full_consensus)) {
    //         assert!(chain.last().unwrap_or(&BTreeMap::new()).keys().len() >=
    //                 chain.len(),
    //                 "round robin with n validators should converge in at most n messages")
    //     }
    // }

    // proptest! {
    //     #![proptest_config(Config::with_cases(10))]
    //     #[test]
    //     fn round_robin_integer(ref chain in chain(prop::num::u32::ANY.boxed(), 2000, round_robin, all_receivers, full_consensus)) {
    //         // total messages until unilateral consensus
    //         println!("{} validators -> {:?} message(s)",
    //                  match chain.last().unwrap_or(&BTreeMap::new()).keys().len().to_string().as_ref()
    //                  {"0" => "Unknown",
    //                   a => a},
    //                  chain.len() + 1);
    //         assert!(chain.last().unwrap_or(&BTreeMap::new()).keys().len() >=
    //                 chain.len(),
    //                 "round robin with n validators should converge in at most n messages")
    //     }
    // }

    // proptest! {
    //     #![proptest_config(Config::with_cases(1))]
    //     #[test]
    //     fn arbitrary_messenger_vote_count(ref chain in chain(VoteCount::arbitrary(), 8, arbitrary_in_set, some_receivers, full_consensus)) {
    //         // total messages until unilateral consensus
    //         println!("{} validators -> {:?} message(s)",
    //                  match chain.last().unwrap_or(&BTreeMap::new()).keys().len().to_string().as_ref()
    //                  {"0" => "Unknown",
    //                   a => a},
    //                  chain.len() + 1);
    //     }
    // }

    // proptest! {
    //     #![proptest_config(Config::with_cases(1))]
    //     #[test]
    //     fn arbitrary_messenger_binary(ref chain in chain(prop::bool::ANY.boxed(), 100, arbitrary_in_set, some_receivers, full_consensus)) {
    //         // total messages until unilateral consensus
    //         println!("{} validators -> {:?} message(s)",
    //                  match chain.last().unwrap_or(&BTreeMap::new()).keys().len().to_string().as_ref()
    //                  {"0" => "Unknown",
    //                   a => a},
    //                  chain.len() + 1);
    //     }
    // }

    // proptest! {
    //     #![proptest_config(Config::with_cases(1))]
    //     #[test]
    //     fn arbitrary_messenger_integer(ref chain in chain(prop::num::u32::ANY.boxed(), 50, arbitrary_in_set, some_receivers, full_consensus)) {
    //         // total messages until unilateral consensus
    //         println!("{} validators -> {:?} message(s)",
    //                  match chain.last().unwrap_or(&BTreeMap::new()).keys().len().to_string().as_ref()
    //                  {"0" => "Unknown",
    //                   a => a},
    //                  chain.len() + 1);
    //     }
    // }

    fn increment(basis: u32) -> BoxedStrategy<u32> {
        (1..2u32).prop_map(move |int| int + basis).boxed()
    }

    #[test]
    fn increment_int() {
        let mut runner = TestRunner::default();
        let mut val = (0..10u32).new_value(&mut runner).unwrap().current();
        println!("{:?}", val);
        val = increment(val).new_value(&mut runner).unwrap().current();
        println!("{:?}", val);
    }

    prop_compose! {
        fn votes(senders: usize, equivocations: usize)
            (votes in prop::collection::vec(prop::bool::weighted(0.3), senders as usize),
             equivocations in Just(equivocations),
             senders in Just(senders))
             -> (Vec<Message<VoteCount, u32>>, HashSet<u32>, usize)
        {
            let mut validators: Vec<u32> = (0..senders as u32).collect();
            thread_rng().shuffle(&mut validators);
            let equivocators: HashSet<u32> = HashSet::from_iter(validators[0..equivocations].iter().cloned());

            let mut messages = vec![];
            votes
                .iter()
                .enumerate()
                .for_each(|(sender, vote)|
                          {messages.push(VoteCount::create_vote_msg(sender as u32, vote.clone()))});
            equivocators
                .iter()
                .for_each(|equivocator|
                          {let vote = !votes[*equivocator as usize];
                           messages.push(VoteCount::create_vote_msg(*equivocator as u32, vote))});
            (messages, equivocators, senders)
        }
    }

    proptest! {
        #![proptest_config(Config::with_cases(1000))]
        #[test]
        fn detect_equivocation(ref vote_tuple in votes(5, 5)) {
            let (messages, equivocators, nodes) = vote_tuple;
            let nodes = nodes.clone();
            let senders: Vec<u32> = (0..nodes as u32).collect();
            let weights: Vec<f64> =
                iter::repeat(1.0).take(nodes as usize).collect();
            let senders_weights = SendersWeight::new(
                senders
                    .iter()
                    .cloned()
                    .zip(weights.iter().cloned())
                    .collect(),
            );
            let sender_state = SenderState::new(
                senders_weights.clone(),
                0.0,
                None,
                LatestMsgs::new(),
                0.0,
                HashSet::new(),
            );

            // here, only take one equivocation
            let single_equivocation: Vec<_> = messages[..nodes+1].iter().map(|message| message).collect();
            let equivocator = messages[nodes].get_sender();
            let (m0, _) =
                &Message::from_msgs(0, single_equivocation.clone(), None, &sender_state, None)
                .unwrap();
            let equivocations: Vec<_> = single_equivocation.iter().filter(|message| message.equivocates(&m0)).collect();
            assert!(if *equivocator == 0 {equivocations.len() == 1} else {equivocations.len() == 0}, "should detect sender 0 equivocating if sender 0 equivocates");
            // the following commented test should fail
            // assert_eq!(equivocations.len(), 1, "should detect sender 0 equivocating if sender 0 equivocates");

            let (m0, _) =
                &Message::from_msgs(0, messages.iter().map(|message| message).collect(), None, &sender_state, None)
                .unwrap();
            let equivocations: Vec<_> = messages.iter().filter(|message| message.equivocates(&m0)).collect();
            assert_eq!(equivocations.len(), 1, "should detect sender 0 equivocating if sender 0 equivocates");

            let sender_state = SenderState::new(
                senders_weights,
                0.0,
                None,
                LatestMsgs::new(),
                equivocators.len() as f64,
                HashSet::new(),
            );
            let (m0, _) =
                &Message::from_msgs(0, messages.iter().map(|message| message).collect(), None, &sender_state, None)
                .unwrap();
            let equivocations: Vec<_> = messages.iter().filter(|message| message.equivocates(&m0)).collect();
            assert_eq!(equivocations.len(), 0, "equivocation absorbed in threshold");
        }
    }

    #[test]
    fn debug() {
        let v0 = VoteCount::create_vote_msg(0, false);
        println!("{:?}", v0);
    }

    #[test]
    fn msg_equality() {
        // v0 and v0_prime are equivocating messages (first child of a fork).
        let v0 = &VoteCount::create_vote_msg(0, false);
        let v1 = &VoteCount::create_vote_msg(1, true);
        let v0_prime = &VoteCount::create_vote_msg(0, true); // equivocating vote
        let v0_idem = &VoteCount::create_vote_msg(0, false);
        assert!(v0 == v0_idem, "v0 and v0_idem should be equal");
        assert!(v0 != v0_prime, "v0 and v0_prime should NOT be equal");
        assert!(v0 != v1, "v0 and v1 should NOT be equal");

        let senders_weights = SendersWeight::new(
            [(0, 1.0), (1, 1.0), (2, 1.0)].iter().cloned().collect(),
        );
        let sender_state = SenderState::new(
            senders_weights,
            0.0,
            None,
            LatestMsgs::new(),
            0.0,
            HashSet::new(),
        );
        let mut j0 = Justification::new();
        let (success, _) = j0
            .faulty_inserts(vec![v0].iter().cloned().collect(), &sender_state);
        assert!(success);

        let external_data: Option<VoteCount> = None;
        let (m0, _) = &Message::from_msgs(
            0,
            vec![v0],
            None,
            &sender_state,
            external_data,
        ).unwrap();
        // let m0 = &Message::new(0, justification, estimate);

        let mut j1 = Justification::new();
        let (success, _) = j1
            .faulty_inserts(vec![v0].iter().cloned().collect(), &sender_state);
        assert!(success);
        let (success, _) = j1
            .faulty_inserts(vec![m0].iter().cloned().collect(), &sender_state);
        assert!(success);

        let (msg1, _) =
            Message::from_msgs(0, vec![v0], None, &sender_state, None).unwrap();
        let (msg2, _) =
            Message::from_msgs(0, vec![v0], None, &sender_state, None).unwrap();
        assert!(msg1 == msg2, "messages should be equal");
        let (msg3, _) =
            Message::from_msgs(0, vec![v0, m0], None, &sender_state, None)
                .unwrap();
        assert!(msg1 != msg3, "msg1 should be different than msg3");
    }

    #[test]
    fn msg_depends() {
        let v0 = &VoteCount::create_vote_msg(0, false);
        let v0_prime = &VoteCount::create_vote_msg(0, true); // equivocating vote

        let senders_weights = SendersWeight::new(
            [(0, 1.0), (1, 1.0), (2, 1.0)].iter().cloned().collect(),
        );
        let sender_state = SenderState::new(
            senders_weights,
            (0.0),
            None,
            LatestMsgs::new(),
            0.0,
            HashSet::new(),
        );

        let mut j0 = Justification::new();
        let (success, _) = j0
            .faulty_inserts(vec![v0].iter().cloned().collect(), &sender_state);
        assert!(success);
        let (m0, _) =
            &Message::from_msgs(0, vec![v0], None, &sender_state, None)
                .unwrap();
        assert!(
            !v0.depends(v0_prime),
            "v0 does NOT depends on v0_prime as they are equivocating"
        );
        assert!(
            !m0.depends(m0),
            "m0 does NOT depends on itself directly, by our impl choice"
        );
        assert!(!m0.depends(v0_prime), "m0 depends on v0 directly");
        assert!(m0.depends(v0), "m0 depends on v0 directly");

        let mut j0 = Justification::new();
        let (success, _) =
            j0.faulty_inserts([v0].iter().cloned().collect(), &sender_state);
        assert!(success);
        let (m0, _) =
            &Message::from_msgs(0, vec![v0], None, &sender_state, None)
                .unwrap();

        let mut j1 = Justification::new();
        let (success, _) =
            j1.faulty_inserts([v0].iter().cloned().collect(), &sender_state);
        assert!(success);
        let (success, _) =
            j1.faulty_inserts([m0].iter().cloned().collect(), &sender_state);
        assert!(success);

        let (m1, _) =
            &Message::from_msgs(0, vec![v0, m0], None, &sender_state, None)
                .unwrap();
        assert!(m1.depends(m0), "m1 DOES depent on m0");
        assert!(!m0.depends(m1), "but m0 does NOT depend on m1");
        assert!(m1.depends(v0), "m1 depends on v0 through m0");
    }

    #[test]
    fn msg_equivocates() {
        let v0 = &VoteCount::create_vote_msg(0, false);
        let v0_prime = &VoteCount::create_vote_msg(0, true); // equivocating vote
        let v1 = &VoteCount::create_vote_msg(1, true);

        let senders_weights = SendersWeight::new(
            [(0, 1.0), (1, 1.0), (2, 1.0)].iter().cloned().collect(),
        );
        let sender_state = SenderState::new(
            senders_weights,
            (0.0),
            None,
            LatestMsgs::new(),
            0.0,
            HashSet::new(),
        );

        let mut j0 = Justification::new();
        let (success, _) = j0
            .faulty_inserts(vec![v0].iter().cloned().collect(), &sender_state);
        assert!(success);
        let (m0, _) =
            &Message::from_msgs(0, vec![v0], None, &sender_state, None)
                .unwrap();

        let (m1, _) =
            &Message::from_msgs(1, vec![v0], None, &sender_state, None)
                .unwrap();
        assert!(!v0.equivocates(v0), "should be all good");
        assert!(!v1.equivocates(m0), "should be all good");
        assert!(!m0.equivocates(v1), "should be all good");
        assert!(v0.equivocates(v0_prime), "should be a direct equivocation");
        assert!(
            m0.equivocates(v0_prime),
            "should be an indirect equivocation, equivocates to m0 through v0"
        );

        assert!(
            m1.equivocates_indirect(v0_prime, HashSet::new()).0,
            "should be an indirect equivocation, equivocates to m0 through v0"
        );
    }
    #[test]
    fn msg_safe_by_weight() {
        let sender0 = 0;
        let sender1 = 1;
        let senders_weights = &SendersWeight::new(
            [(sender0, 0.5), (sender1, 0.5)].iter().cloned().collect(),
        );

        let sender_state = SenderState::new(
            senders_weights.clone(),
            (0.0),
            None,
            LatestMsgs::new(),
            0.0,
            HashSet::new(),
        );

        let thr = &0.5;
        // sender0        v0---m0        m2---
        // sender1               \--m1--/
        let v0 = &VoteCount::create_vote_msg(sender0, false);
        let safe_msgs = v0.get_safe_msgs_by_weight(senders_weights, thr);
        assert_eq!(safe_msgs.len(), 0, "only 0.5 of weight saw v0");

        let (m0, _) = &Message::from_msgs(
            sender0,
            vec![&v0],
            None,
            &sender_state,
            None as Option<VoteCount>,
        ).unwrap();
        let safe_msgs = m0.get_safe_msgs_by_weight(senders_weights, thr);
        assert_eq!(safe_msgs.len(), 0, "only 0.5 of weight saw v0 and m0");

        let (m1, _) = &Message::from_msgs(
            sender1,
            vec![&m0],
            None,
            &sender_state,
            None as Option<VoteCount>,
        ).unwrap();
        let safe_msgs = m1.get_safe_msgs_by_weight(senders_weights, thr);
        assert_eq!(
            safe_msgs.len(),
            0,
            "both sender0 (0.5) and sender1 (0.5) saw v0 and m0, but sender0 hasn't
necessarly seen sender1 seeing v0 and m0, thus not yet safe"
        );

        let (m2, _) = &Message::from_msgs(
            sender0,
            vec![&m1],
            None,
            &sender_state,
            None as Option<VoteCount>,
        ).unwrap();
        let safe_msgs = m2.get_safe_msgs_by_weight(senders_weights, thr);
        assert_eq!(
            safe_msgs.get(m0).unwrap_or(&f64::NAN),
            &1.0,
            "both sender0 and sender1 saw v0 and m0, and additionally both
parties saw each other seing v0 and m0, m0 (and all its dependencies) are final"
        );
        // let senders = &Sender::get_senders(&relative_senders_weights);
    }
    #[test]
    fn set_as_final() {
        let sender0 = 0;
        let sender1 = 1;
        let senders_weights = SendersWeight::new(
            [(sender0, 1.0), (sender1, 1.0)].iter().cloned().collect(),
        );
        let sender_state = SenderState::new(
            senders_weights.clone(),
            (0.0),
            None,
            LatestMsgs::new(),
            0.0,
            HashSet::new(),
        );
        let senders = &senders_weights.get_senders().unwrap();

        // sender0        v0---m0        m2---
        // sender1               \--m1--/
        let v0 = &VoteCount::create_vote_msg(sender1, false);
        let safe_msgs = v0.get_msg_for_proposition(senders);
        assert_eq!(safe_msgs.len(), 0, "only sender0 saw v0");

        let (m0, sender_state) = &mut Message::from_msgs(
            sender0,
            vec![v0],
            None,
            &sender_state,
            None as Option<VoteCount>,
        ).unwrap();

        let (m1, sender_state) = &Message::from_msgs(
            sender1,
            vec![m0],
            None,
            &sender_state,
            None as Option<VoteCount>,
        ).unwrap();
        let (m2, sender_state) = &Message::from_msgs(
            sender0,
            vec![m1],
            None,
            &sender_state,
            None as Option<VoteCount>,
        ).unwrap();

        let safe_msgs = m2.get_msg_for_proposition(senders);

        assert!(safe_msgs.len() == 1);
        println!("------------");
        println!("message before trimmed by set_as_final\n {:?}", m0);
        m0.set_as_final();
        println!("message after\n {:?}", m0);
        println!("------------");
    }

    #[test]
    fn msg_safe_by_sender() {
        // setup
        let sender0 = 0;
        let sender1 = 1;
        let senders_weights = SendersWeight::new(
            [(sender0, 1.0), (sender1, 1.0)].iter().cloned().collect(),
        );
        let sender_state = SenderState::new(
            senders_weights.clone(),
            0.0,
            None,
            LatestMsgs::new(),
            0.0,
            HashSet::new(),
        );
        let senders = &senders_weights.get_senders().unwrap();

        // sender0        v0---m0        m2---
        // sender1               \--m1--/
        let v0 = &VoteCount::create_vote_msg(sender0, false);
        let safe_msgs = v0.get_msg_for_proposition(senders);
        assert_eq!(safe_msgs.len(), 0, "only sender0 saw v0");

        let (m0, sender_state0) = &Message::from_msgs(
            sender0,
            vec![v0],
            None,
            &sender_state,
            None as Option<VoteCount>,
        ).unwrap();
        let safe_msgs = m0.get_msg_for_proposition(senders);
        assert_eq!(safe_msgs.len(), 0, "only sender0 saw v0 and m0");

        let (m1, sender_state1) = &Message::from_msgs(
            sender1,
            vec![m0],
            None,
            &sender_state,
            None as Option<VoteCount>,
        ).unwrap();
        let safe_msgs = m1.get_msg_for_proposition(senders);
        assert_eq!(
            safe_msgs.len(),
            0,
            "both sender0 and sender1 saw v0 and m0, but sender0 hasn't
necessarly seen sender1 seeing v0 and m0, thus not yet safe"
        );

        let (m2, sender_state0) = &Message::from_msgs(
            sender0,
            vec![m1],
            None,
            &sender_state0,
            None as Option<VoteCount>,
        ).unwrap();
        let safe_msgs = m2.get_msg_for_proposition(senders);
        assert_eq!(
            safe_msgs,
            [m0.clone()].iter().cloned().collect(),
            "both sender0 and sender1 saw v0 and m0, and additionally both
parties saw each other seing v0 and m0, m0 (and all its dependencies) are final"
        );

        // sender0        v0---m0        m2---
        // sender1        v1--/   \--m1--/
        let v0 = &VoteCount::create_vote_msg(sender0, false);
        let v1 = &VoteCount::create_vote_msg(sender1, false);
        let (ref mut m0, sender_state0) = &mut Message::from_msgs(
            sender0,
            vec![v0, v1],
            None,
            &sender_state,
            None as Option<VoteCount>,
        ).unwrap();
        let safe_msgs = m0.get_msg_for_proposition(senders);
        assert_eq!(
            safe_msgs.len(),
            0,
            "sender0 saw v0, v1 and m0, and sender1 saw only v1"
        );

        let (m1, sender_state1) = &Message::from_msgs(
            sender1,
            vec![m0],
            None,
            &sender_state,
            None as Option<VoteCount>,
        ).unwrap();
        let safe_msgs = m1.get_msg_for_proposition(senders);

        assert_eq!(
            safe_msgs,
            [v1.clone()].iter().cloned().collect(),
            "both sender0 and sender1 saw v0, v1 and m0, but sender0 hasn't
necessarly seen sender1 seeing v0 and m0, just v1 is safe"
        );

        let (m2, _sender_state) = &Message::from_msgs(
            sender0,
            vec![m1],
            None,
            &sender_state0,
            None as Option<VoteCount>,
        ).unwrap();
        let safe_msgs = m2.get_msg_for_proposition(senders);
        assert_eq!(
            safe_msgs,
            [m0.clone()].iter().cloned().collect(),
            "both sender0 and sender1 saw v0 and m0, and additionally both
parties saw each other seing v0 and m0, safe"
        );
        m0.set_as_final()
    }
}
