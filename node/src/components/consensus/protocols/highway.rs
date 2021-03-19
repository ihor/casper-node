pub(crate) mod config;
mod participation;
mod round_success_meter;
mod synchronizer;
#[cfg(test)]
mod tests;

use std::{
    any::Any,
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Debug,
    iter,
    path::PathBuf,
};

use datasize::DataSize;
use itertools::Itertools;
use num_traits::AsPrimitive;
use serde::{Deserialize, Serialize};
use tracing::{error, info, trace, warn};

use casper_types::{system::auction::BLOCK_REWARD, U512};

use crate::{
    components::consensus::{
        config::{Config, ProtocolConfig},
        consensus_protocol::{BlockContext, ConsensusProtocol, ProtocolOutcome, ProtocolOutcomes},
        highway_core::{
            active_validator::Effect as AvEffect,
            finality_detector::{FinalityDetector, FttExceeded},
            highway::{Dependency, GetDepOutcome, Highway, Params, ValidVertex, Vertex},
            state::{Observation, Panorama},
            validators::{ValidatorIndex, Validators},
        },
        traits::{ConsensusValueT, Context, NodeIdT},
        ActionId, TimerId,
    },
    types::{TimeDiff, Timestamp},
};

pub use self::config::Config as HighwayConfig;
use self::{round_success_meter::RoundSuccessMeter, synchronizer::Synchronizer};

/// Never allow more than this many units in a piece of evidence for conflicting endorsements,
/// even if eras are longer than this.
const MAX_ENDORSEMENT_EVIDENCE_LIMIT: u64 = 10_000;

/// The timer for creating new units, as a validator actively participating in consensus.
const TIMER_ID_ACTIVE_VALIDATOR: TimerId = TimerId(0);
/// The timer for adding a vertex with a future timestamp.
const TIMER_ID_VERTEX_WITH_FUTURE_TIMESTAMP: TimerId = TimerId(1);
/// The timer for purging expired pending vertices from the queues.
const TIMER_ID_PURGE_VERTICES: TimerId = TimerId(2);
/// The timer for logging inactive validators.
const TIMER_ID_LOG_PARTICIPATION: TimerId = TimerId(3);
/// The timer for an alert no progress was made in a long time.
const TIMER_ID_STANDSTILL_ALERT: TimerId = TimerId(4);

/// The action of adding a vertex from the `vertices_to_be_added` queue.
const ACTION_ID_VERTEX: ActionId = ActionId(0);

#[derive(DataSize, Debug)]
pub(crate) struct HighwayProtocol<I, C>
where
    I: DataSize,
    C: Context,
{
    /// Incoming blocks we can't add yet because we are waiting for validation.
    pending_values: HashMap<<C::ConsensusValue as ConsensusValueT>::Hash, Vec<ValidVertex<C>>>,
    finality_detector: FinalityDetector<C>,
    highway: Highway<C>,
    /// A tracker for whether we are keeping up with the current round exponent or not.
    round_success_meter: RoundSuccessMeter<C>,
    synchronizer: Synchronizer<I, C>,
    evidence_only: bool,
    /// The panorama snapshot. This is updated periodically, and if it does not change for too
    /// long, an alert is raised.
    last_panorama: Panorama<C>,
    /// If the current era's protocol state has not progressed for this long, return
    /// `ProtocolOutcome::StandstillAlert`.
    standstill_timeout: TimeDiff,
    /// Log inactive or faulty validators periodically, with this interval.
    log_participation_interval: TimeDiff,
}

impl<I: NodeIdT, C: Context + 'static> HighwayProtocol<I, C> {
    /// Creates a new boxed `HighwayProtocol` instance.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub(crate) fn new_boxed(
        instance_id: C::InstanceId,
        validator_stakes: BTreeMap<C::ValidatorId, U512>,
        slashed: &HashSet<C::ValidatorId>,
        protocol_config: &ProtocolConfig,
        config: &Config,
        prev_cp: Option<&dyn ConsensusProtocol<I, C>>,
        era_start_time: Timestamp,
        seed: u64,
        now: Timestamp,
    ) -> (Box<dyn ConsensusProtocol<I, C>>, ProtocolOutcomes<I, C>) {
        let sum_stakes: U512 = validator_stakes.iter().map(|(_, stake)| *stake).sum();
        assert!(
            !sum_stakes.is_zero(),
            "cannot start era with total weight 0"
        );
        // For Highway, we need u64 weights. Scale down by  sum / u64::MAX,  rounded up.
        // If we round up the divisor, the resulting sum is guaranteed to be  <= u64::MAX.
        let scaling_factor = (sum_stakes + U512::from(u64::MAX) - 1) / U512::from(u64::MAX);
        let scale_stake = |(key, stake): (C::ValidatorId, U512)| {
            (key, AsPrimitive::<u64>::as_(stake / scaling_factor))
        };
        let mut validators: Validators<C::ValidatorId> =
            validator_stakes.into_iter().map(scale_stake).collect();

        for vid in slashed {
            validators.ban(vid);
        }

        let highway_config = &protocol_config.highway_config;

        let total_weight = u128::from(validators.total_weight());
        let ftt_fraction = highway_config.finality_threshold_fraction;
        let ftt = ((total_weight * *ftt_fraction.numer() as u128 / *ftt_fraction.denom() as u128)
            as u64)
            .into();

        let init_round_exp = prev_cp
            .and_then(|cp| cp.as_any().downcast_ref::<HighwayProtocol<I, C>>())
            .and_then(|highway_proto| highway_proto.our_round_exp())
            .unwrap_or(highway_config.minimum_round_exponent);

        info!(
            %init_round_exp,
            "initializing Highway instance",
        );

        // Allow about as many units as part of evidence for conflicting endorsements as we expect
        // a validator to create during an era. After that, they can endorse two conflicting forks
        // without getting slashed.
        let min_round_len = 1 << highway_config.minimum_round_exponent;
        let min_rounds_per_era = protocol_config
            .minimum_era_height
            .max(1 + protocol_config.era_duration.millis() / min_round_len);
        let endorsement_evidence_limit =
            (2 * min_rounds_per_era).min(MAX_ENDORSEMENT_EVIDENCE_LIMIT);

        let params = Params::new(
            seed,
            BLOCK_REWARD,
            (highway_config.reduced_reward_multiplier * BLOCK_REWARD).to_integer(),
            highway_config.minimum_round_exponent,
            highway_config.maximum_round_exponent,
            init_round_exp,
            protocol_config.minimum_era_height,
            era_start_time,
            era_start_time + protocol_config.era_duration,
            endorsement_evidence_limit,
        );

        let mut outcomes = Self::initialize_timers(now, era_start_time, &config.highway);

        // Request the latest state from peers on startup.
        // We will catch up with the consensus state and also sync our own unit in case we
        // restarted. Nodes might have gone into "sleep mode" so we won't get the panorama
        // in a different way.
        let latest_state_request =
            HighwayMessage::LatestStateRequest::<C>(Panorama::new(validators.len()));

        outcomes.push(ProtocolOutcome::CreatedGossipMessage(
            (&latest_state_request).serialize(),
        ));

        let min_round_exp = params.min_round_exp();
        let max_round_exp = params.max_round_exp();
        let round_exp = params.init_round_exp();
        let start_timestamp = params.start_timestamp();
        let round_success_meter = prev_cp
            .and_then(|cp| cp.as_any().downcast_ref::<HighwayProtocol<I, C>>())
            .map(|highway_proto| highway_proto.next_era_round_succ_meter(start_timestamp))
            .unwrap_or_else(|| {
                RoundSuccessMeter::new(
                    round_exp,
                    min_round_exp,
                    max_round_exp,
                    start_timestamp,
                    config.into(),
                )
            });
        let highway = Highway::new(instance_id, validators, params);
        let last_panorama = highway.state().panorama().clone();
        let hw_proto = Box::new(HighwayProtocol {
            pending_values: HashMap::new(),
            finality_detector: FinalityDetector::new(ftt),
            highway,
            round_success_meter,
            synchronizer: Synchronizer::new(config.highway.pending_vertex_timeout),
            evidence_only: false,
            last_panorama,
            standstill_timeout: config.highway.standstill_timeout,
            log_participation_interval: config.highway.log_participation_interval,
        });

        (hw_proto, outcomes)
    }

    fn initialize_timers(
        now: Timestamp,
        era_start_time: Timestamp,
        highway_config: &HighwayConfig,
    ) -> ProtocolOutcomes<I, C> {
        vec![
            ProtocolOutcome::ScheduleTimer(
                now + highway_config.pending_vertex_timeout,
                TIMER_ID_PURGE_VERTICES,
            ),
            ProtocolOutcome::ScheduleTimer(
                now.max(era_start_time) + highway_config.log_participation_interval,
                TIMER_ID_LOG_PARTICIPATION,
            ),
            ProtocolOutcome::ScheduleTimer(
                now.max(era_start_time) + highway_config.standstill_timeout,
                TIMER_ID_STANDSTILL_ALERT,
            ),
        ]
    }

    fn process_av_effects<E>(&mut self, av_effects: E, now: Timestamp) -> ProtocolOutcomes<I, C>
    where
        E: IntoIterator<Item = AvEffect<C>>,
    {
        av_effects
            .into_iter()
            .flat_map(|effect| self.process_av_effect(effect, now))
            .collect()
    }

    fn process_av_effect(&mut self, effect: AvEffect<C>, now: Timestamp) -> ProtocolOutcomes<I, C> {
        match effect {
            AvEffect::NewVertex(vv) => {
                self.calculate_round_exponent(&vv, now);
                self.process_new_vertex(vv.into())
            }
            AvEffect::ScheduleTimer(timestamp) => {
                vec![ProtocolOutcome::ScheduleTimer(
                    timestamp,
                    TIMER_ID_ACTIVE_VALIDATOR,
                )]
            }
            AvEffect::RequestNewBlock {
                block_context,
                fork_choice,
            } => {
                let past_values = self.non_finalized_values(fork_choice).cloned().collect();
                vec![ProtocolOutcome::CreateNewBlock {
                    block_context,
                    past_values,
                }]
            }
            AvEffect::WeAreFaulty(fault) => {
                error!("this validator is faulty: {:?}", fault);
                vec![ProtocolOutcome::WeAreFaulty]
            }
        }
    }

    fn process_new_vertex(&mut self, v: Vertex<C>) -> ProtocolOutcomes<I, C> {
        let mut outcomes = Vec::new();
        if let Vertex::Evidence(ev) = &v {
            let v_id = self
                .highway
                .validators()
                .id(ev.perpetrator())
                .expect("validator not found")
                .clone();
            outcomes.push(ProtocolOutcome::NewEvidence(v_id));
        }
        let msg = HighwayMessage::NewVertex(v);
        outcomes.push(ProtocolOutcome::CreatedGossipMessage(msg.serialize()));
        outcomes.extend(self.detect_finality());
        outcomes
    }

    fn detect_finality(&mut self) -> ProtocolOutcomes<I, C> {
        let faulty_weight = match self.finality_detector.run(&self.highway) {
            Ok(iter) => return iter.map(ProtocolOutcome::FinalizedBlock).collect(),
            Err(FttExceeded(weight)) => weight.0,
        };
        error!(
            %faulty_weight,
            total_weight = %self.highway.state().total_weight().0,
            "too many faulty validators"
        );
        self.log_participation();
        vec![ProtocolOutcome::FttExceeded]
    }

    /// Adds the given vertices to the protocol state, if possible, or requests missing
    /// dependencies or validation. Recursively schedules events to add everything that is
    /// unblocked now.
    fn add_vertex(&mut self, now: Timestamp) -> ProtocolOutcomes<I, C> {
        let (maybe_pending_vertex, mut outcomes) =
            self.synchronizer.pop_vertex_to_add(&self.highway);
        let pending_vertex = match maybe_pending_vertex {
            None => return outcomes,
            Some(pending_vertex) => pending_vertex,
        };

        // If unit is sent by a doppelganger, deactivate this instance of an active
        // validator. Continue processing the unit so that it can be added to the state.
        if self.highway.is_doppelganger_vertex(pending_vertex.vertex()) {
            error!(
                "received vertex from a doppelganger. \
                 Are you running multiple nodes with the same validator key?",
            );
            self.deactivate_validator();
            outcomes.push(ProtocolOutcome::DoppelgangerDetected);
        }

        // If the vertex is invalid, drop all vertices that depend on this one, and disconnect from
        // the faulty senders.
        let sender = pending_vertex.sender().clone();
        let vv = match self.highway.validate_vertex(pending_vertex.into()) {
            Ok(vv) => vv,
            Err((pvv, err)) => {
                info!(?pvv, ?err, "invalid vertex");
                let vertices = vec![pvv.inner().id()];
                let faulty_senders = self.synchronizer.drop_dependent_vertices(vertices);
                outcomes.extend(faulty_senders.into_iter().map(ProtocolOutcome::Disconnect));
                return outcomes;
            }
        };

        // If the vertex contains a consensus value, request validation.
        let vertex = vv.inner();
        if let (Some(value), Some(timestamp)) = (vertex.value(), vertex.timestamp()) {
            if value.needs_validation() {
                let cv = value.clone();
                self.pending_values
                    .entry(value.hash())
                    .or_default()
                    .push(vv);
                outcomes.push(ProtocolOutcome::ValidateConsensusValue(
                    sender, cv, timestamp,
                ));
                return outcomes;
            }
        }

        // Either consensus value doesn't need validation or it's not a proposal.
        // We can add it to the state.
        outcomes.extend(self.add_valid_vertex(vv, now));
        // If we added new vertices to the state, check whether any dependencies we were
        // waiting for are now satisfied, and try adding the pending vertices as well.
        outcomes.extend(self.synchronizer.remove_satisfied_deps(&self.highway));
        // Check whether any new blocks were finalized.
        outcomes.extend(self.detect_finality());
        outcomes
    }

    fn calculate_round_exponent(&mut self, vv: &ValidVertex<C>, now: Timestamp) {
        let new_round_exp = self
            .round_success_meter
            .calculate_new_exponent(self.highway.state());
        // If the vertex contains a proposal, register it in the success meter.
        // It's important to do this _after_ the calculation above - otherwise we might try to
        // register the proposal before the meter is aware that a new round has started, and it
        // will reject the proposal.
        if vv.is_proposal() {
            let vertex = vv.inner();
            if let (Some(hash), Some(timestamp)) = (vertex.unit_hash(), vertex.timestamp()) {
                trace!(%now, timestamp = timestamp.millis(), "adding proposal to protocol state");
                self.round_success_meter.new_proposal(hash, timestamp);
            } else {
                error!(?vertex, "proposal without unit hash and timestamp");
            }
        }
        self.highway.set_round_exp(new_round_exp);
    }

    fn add_valid_vertex(&mut self, vv: ValidVertex<C>, now: Timestamp) -> ProtocolOutcomes<I, C> {
        if self.evidence_only && !vv.inner().is_evidence() {
            error!(vertex = ?vv.inner(), "unexpected vertex in evidence-only mode");
            return vec![];
        }
        // Check whether we should change the round exponent.
        // It's important to do it before the vertex is added to the state - this way if the last
        // round has finished, we now have all the vertices from that round in the state, and no
        // newer ones.
        self.calculate_round_exponent(&vv, now);
        let av_effects = self.highway.add_valid_vertex(vv, now);
        self.process_av_effects(av_effects, now)
    }

    /// Returns our most recent round exponent.
    /// Returns `None` if we created no units in this era.
    fn our_round_exp(&self) -> Option<u8> {
        self.highway.latest_own_unit().map(|unit| unit.round_exp)
    }

    /// Returns an instance of `RoundSuccessMeter` for the new era: resetting the counters where
    /// appropriate.
    fn next_era_round_succ_meter(&self, era_start_timestamp: Timestamp) -> RoundSuccessMeter<C> {
        self.round_success_meter.next_era(era_start_timestamp)
    }

    /// Returns an iterator over all the values that are expected to become finalized, but are not
    /// finalized yet.
    fn non_finalized_values(
        &self,
        mut fork_choice: Option<C::Hash>,
    ) -> impl Iterator<Item = &C::ConsensusValue> {
        let last_finalized = self.finality_detector.last_finalized();
        iter::from_fn(move || {
            if fork_choice.as_ref() == last_finalized {
                return None;
            }
            let maybe_block = fork_choice.map(|bhash| self.highway.state().block(&bhash));
            let value = maybe_block.map(|block| &block.value);
            fork_choice = maybe_block.and_then(|block| block.parent().cloned());
            value
        })
    }

    /// Prints a log statement listing the inactive and faulty validators.
    fn log_participation(&self) {
        let instance_id = self.highway.instance_id();
        let participation = participation::Participation::new(&self.highway);
        info!(?participation, %instance_id, "validator participation");
    }

    /// Returns whether the switch block has already been finalized.
    fn finalized_switch_block(&self) -> bool {
        let is_switch = |block_hash: &C::Hash| self.highway.state().is_terminal_block(block_hash);
        self.finality_detector
            .last_finalized()
            .map_or(false, is_switch)
    }

    /// Returns a `StandstillAlert` if no progress was made; otherwise schedules the next check.
    fn handle_standstill_alert_timer(&mut self, now: Timestamp) -> ProtocolOutcomes<I, C> {
        if self.evidence_only || self.finalized_switch_block() {
            return vec![]; // Era has ended. No further progress is expected.
        }
        if self.last_panorama == *self.highway.state().panorama() {
            return vec![ProtocolOutcome::StandstillAlert]; // No progress within the timeout.
        }
        // Record the current panorama and schedule the next standstill check.
        self.last_panorama = self.highway.state().panorama().clone();
        vec![ProtocolOutcome::ScheduleTimer(
            now + self.standstill_timeout,
            TIMER_ID_STANDSTILL_ALERT,
        )]
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(bound(
    serialize = "C::Hash: Serialize",
    deserialize = "C::Hash: Deserialize<'de>",
))]
enum HighwayMessage<C: Context> {
    NewVertex(Vertex<C>),
    RequestDependency(Dependency<C>),
    LatestStateRequest(Panorama<C>),
}

impl<C: Context> HighwayMessage<C> {
    fn serialize(&self) -> Vec<u8> {
        bincode::serialize(self).expect("should serialize message")
    }
}

impl<I, C> ConsensusProtocol<I, C> for HighwayProtocol<I, C>
where
    I: NodeIdT,
    C: Context + 'static,
{
    fn handle_message(
        &mut self,
        sender: I,
        msg: Vec<u8>,
        now: Timestamp,
    ) -> ProtocolOutcomes<I, C> {
        match bincode::deserialize(msg.as_slice()) {
            Err(err) => vec![ProtocolOutcome::InvalidIncomingMessage(
                msg,
                sender,
                err.into(),
            )],
            Ok(HighwayMessage::NewVertex(v))
                if self.highway.has_vertex(&v) || (self.evidence_only && !v.is_evidence()) =>
            {
                trace!(
                    has_vertex = self.highway.has_vertex(&v),
                    is_evidence = v.is_evidence(),
                    evidence_only = %self.evidence_only,
                    "received an irrelevant vertex"
                );
                vec![]
            }
            Ok(HighwayMessage::NewVertex(v)) => {
                // Keep track of whether the prevalidated vertex was from an equivocator
                let v_id = v.id();
                let pvv = match self.highway.pre_validate_vertex(v) {
                    Ok(pvv) => pvv,
                    Err((_, err)) => {
                        trace!("received an invalid vertex");
                        // drop the vertices that might have depended on this one
                        let faulty_senders = self.synchronizer.drop_dependent_vertices(vec![v_id]);
                        return iter::once(ProtocolOutcome::InvalidIncomingMessage(
                            msg,
                            sender,
                            err.into(),
                        ))
                        .chain(faulty_senders.into_iter().map(ProtocolOutcome::Disconnect))
                        .collect();
                    }
                };
                let is_faulty = match pvv.inner().creator() {
                    Some(creator) => self.highway.state().is_faulty(creator),
                    None => false,
                };

                if is_faulty && !self.synchronizer.is_dependency(&pvv.inner().id()) {
                    trace!("received a vertex from a faulty validator; dropping");
                    return vec![];
                }

                match pvv.timestamp() {
                    Some(timestamp)
                        if timestamp > now + self.synchronizer.pending_vertex_timeout() =>
                    {
                        trace!("received a vertex with a timestamp far in the future; dropping");
                        vec![]
                    }
                    Some(timestamp) if timestamp > now => {
                        // If it's not from an equivocator and from the future, add to queue
                        trace!("received a vertex from the future; storing for later");
                        self.synchronizer
                            .store_vertex_for_addition_later(timestamp, now, sender, pvv);
                        let timer_id = TIMER_ID_VERTEX_WITH_FUTURE_TIMESTAMP;
                        vec![ProtocolOutcome::ScheduleTimer(timestamp, timer_id)]
                    }
                    _ => {
                        // If it's not from an equivocator or it is a transitive dependency, add the
                        // vertex
                        trace!("received a valid vertex");
                        self.synchronizer.schedule_add_vertex(sender, pvv, now)
                    }
                }
            }
            Ok(HighwayMessage::RequestDependency(dep)) => {
                trace!("received a request for a dependency");
                match self.highway.get_dependency(&dep) {
                    GetDepOutcome::None => {
                        info!(?dep, ?sender, "requested dependency doesn't exist");
                        vec![]
                    }
                    GetDepOutcome::Evidence(vid) => {
                        vec![ProtocolOutcome::SendEvidence(sender, vid)]
                    }
                    // TODO: Should this be done via a gossip service?
                    GetDepOutcome::Vertex(vv) => vec![ProtocolOutcome::CreatedTargetedMessage(
                        HighwayMessage::NewVertex(vv.into()).serialize(),
                        sender,
                    )],
                }
            }
            Ok(HighwayMessage::LatestStateRequest(panorama)) => {
                trace!("received a request for the latest state");
                let state = self.highway.state();

                let create_message =
                    |observations: ((ValidatorIndex, &Observation<C>), &Observation<C>)| {
                        let vid = observations.0 .0;
                        let observations = (observations.0 .1, observations.1);
                        match observations {
                            (obs0, obs1) if obs0 == obs1 => None,

                            (Observation::None, Observation::None) => None,

                            (Observation::Faulty, _) => state.maybe_evidence(vid).map(|evidence| {
                                HighwayMessage::NewVertex(Vertex::Evidence(evidence.clone()))
                            }),

                            (_, Observation::Faulty) => {
                                Some(HighwayMessage::RequestDependency(Dependency::Evidence(vid)))
                            }

                            (Observation::None, Observation::Correct(hash)) => {
                                Some(HighwayMessage::RequestDependency(Dependency::Unit(*hash)))
                            }

                            (Observation::Correct(hash), Observation::None) => state
                                .wire_unit(hash, *self.highway.instance_id())
                                .map(|swu| HighwayMessage::NewVertex(Vertex::Unit(swu))),

                            (Observation::Correct(our_hash), Observation::Correct(their_hash)) => {
                                if state.has_unit(their_hash)
                                    && state.panorama().sees_correct(state, their_hash)
                                {
                                    state
                                        .wire_unit(our_hash, *self.highway.instance_id())
                                        .map(|swu| HighwayMessage::NewVertex(Vertex::Unit(swu)))
                                } else if !state.has_unit(their_hash) {
                                    Some(HighwayMessage::RequestDependency(Dependency::Unit(
                                        *their_hash,
                                    )))
                                } else {
                                    None
                                }
                            }
                        }
                    };

                state
                    .panorama()
                    .enumerate()
                    .zip(&panorama)
                    .filter_map(create_message)
                    .map(|msg| {
                        ProtocolOutcome::CreatedTargetedMessage(msg.serialize(), sender.clone())
                    })
                    .collect()
            }
        }
    }

    fn handle_new_peer(&mut self, peer_id: I) -> ProtocolOutcomes<I, C> {
        trace!(?peer_id, "connected to a new peer");
        let msg = HighwayMessage::LatestStateRequest(self.highway.state().panorama().clone());
        vec![ProtocolOutcome::CreatedTargetedMessage(
            msg.serialize(),
            peer_id,
        )]
    }

    fn handle_timer(&mut self, now: Timestamp, timer_id: TimerId) -> ProtocolOutcomes<I, C> {
        match timer_id {
            TIMER_ID_ACTIVE_VALIDATOR => {
                let effects = self.highway.handle_timer(now);
                self.process_av_effects(effects, now)
            }
            TIMER_ID_VERTEX_WITH_FUTURE_TIMESTAMP => {
                self.synchronizer.add_past_due_stored_vertices(now)
            }
            TIMER_ID_PURGE_VERTICES => {
                self.synchronizer.purge_vertices(now);
                let next_time = now + self.synchronizer.pending_vertex_timeout();
                vec![ProtocolOutcome::ScheduleTimer(next_time, timer_id)]
            }
            TIMER_ID_LOG_PARTICIPATION => {
                self.log_participation();
                if !self.evidence_only && !self.finalized_switch_block() {
                    let next_time = now + self.log_participation_interval;
                    vec![ProtocolOutcome::ScheduleTimer(next_time, timer_id)]
                } else {
                    vec![]
                }
            }
            TIMER_ID_STANDSTILL_ALERT => self.handle_standstill_alert_timer(now),
            _ => unreachable!("unexpected timer ID"),
        }
    }

    fn handle_action(&mut self, action_id: ActionId, now: Timestamp) -> ProtocolOutcomes<I, C> {
        match action_id {
            ACTION_ID_VERTEX => self.add_vertex(now),
            _ => unreachable!("unexpected action ID"),
        }
    }

    fn propose(
        &mut self,
        value: C::ConsensusValue,
        block_context: BlockContext,
        now: Timestamp,
    ) -> ProtocolOutcomes<I, C> {
        let effects = self.highway.propose(value, block_context);
        self.process_av_effects(effects, now)
    }

    fn resolve_validity(
        &mut self,
        value: &C::ConsensusValue,
        valid: bool,
        now: Timestamp,
    ) -> ProtocolOutcomes<I, C> {
        if valid {
            let mut outcomes = self
                .pending_values
                .remove(&value.hash())
                .into_iter()
                .flatten()
                .flat_map(|vv| self.add_valid_vertex(vv, now))
                .collect_vec();
            outcomes.extend(self.synchronizer.remove_satisfied_deps(&self.highway));
            outcomes.extend(self.detect_finality());
            outcomes
        } else {
            // TODO: Slash proposer?
            // Drop vertices dependent on the invalid value.
            let dropped_vertices = self.pending_values.remove(&value.hash());
            // recursively remove vertices depending on the dropped ones
            warn!(
                ?value,
                ?dropped_vertices,
                "consensus value is invalid; dropping dependent vertices"
            );
            let _faulty_senders = self.synchronizer.drop_dependent_vertices(
                dropped_vertices
                    .into_iter()
                    .flatten()
                    .map(|vv| vv.inner().id())
                    .collect(),
            );
            // We don't disconnect from the faulty senders here: The block validator considers the
            // value "invalid" even if it just couldn't download the deploys, which could just be
            // because the original sender went offline.
            vec![]
        }
    }

    fn activate_validator(
        &mut self,
        our_id: C::ValidatorId,
        secret: C::ValidatorSecret,
        now: Timestamp,
        unit_hash_file: Option<PathBuf>,
    ) -> ProtocolOutcomes<I, C> {
        let ftt = self.finality_detector.fault_tolerance_threshold();
        let av_effects = self
            .highway
            .activate_validator(our_id, secret, now, unit_hash_file, ftt);
        self.process_av_effects(av_effects, now)
    }

    fn deactivate_validator(&mut self) {
        self.highway.deactivate_validator()
    }

    fn set_evidence_only(&mut self) {
        // TODO: We could also drop the finality detector and round success meter here. Maybe make
        // HighwayProtocol an enum with an EvidenceOnly variant?
        self.pending_values.clear();
        self.synchronizer.retain_evidence_only();
        self.highway.retain_evidence_only();
        self.evidence_only = true;
    }

    fn has_evidence(&self, vid: &C::ValidatorId) -> bool {
        self.highway.has_evidence(vid)
    }

    fn mark_faulty(&mut self, vid: &C::ValidatorId) {
        self.highway.mark_faulty(vid);
    }

    fn request_evidence(&self, sender: I, vid: &C::ValidatorId) -> ProtocolOutcomes<I, C> {
        self.highway
            .validators()
            .get_index(vid)
            .and_then(
                move |vidx| match self.highway.get_dependency(&Dependency::Evidence(vidx)) {
                    GetDepOutcome::None | GetDepOutcome::Evidence(_) => None,
                    GetDepOutcome::Vertex(vv) => {
                        let msg = HighwayMessage::NewVertex(vv.into());
                        Some(ProtocolOutcome::CreatedTargetedMessage(
                            msg.serialize(),
                            sender,
                        ))
                    }
                },
            )
            .into_iter()
            .collect()
    }

    /// Sets the pause status: While paused we don't create any new units, just pings.
    fn set_paused(&mut self, paused: bool) {
        self.highway.set_paused(paused);
    }

    fn validators_with_evidence(&self) -> Vec<&C::ValidatorId> {
        self.highway.validators_with_evidence().collect()
    }

    fn has_received_messages(&self) -> bool {
        !self.highway.state().is_empty()
            || !self.synchronizer.is_empty()
            || !self.pending_values.is_empty()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_active(&self) -> bool {
        self.highway.is_active()
    }

    fn instance_id(&self) -> &C::InstanceId {
        self.highway.instance_id()
    }

    fn next_round_length(&self) -> Option<TimeDiff> {
        self.highway.next_round_length()
    }
}
