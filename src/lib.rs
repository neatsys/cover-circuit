pub mod ser;

use std::{collections::HashMap, fmt::Debug};

use plonky2::{
    field::types::{Field, PrimeField64},
    hash::{
        hash_types::{HashOut, HashOutTarget},
        hashing::hash_n_to_hash_no_pad,
        poseidon::{PoseidonHash, PoseidonPermutation},
    },
    iop::{
        target::Target,
        witness::{PartialWitness, WitnessWrite},
    },
    plonk::{
        circuit_builder::CircuitBuilder,
        circuit_data::{CircuitConfig, CircuitData, VerifierCircuitTarget},
        config::{GenericConfig, PoseidonGoldilocksConfig},
        proof::{ProofWithPublicInputs, ProofWithPublicInputsTarget},
        prover::prove,
    },
    util::timing::TimingTree,
};
use plonky2_u32::gadgets::{
    arithmetic_u32::U32Target, multiple_comparison::list_le_u32_circuit,
    range_check::range_check_u32_circuit,
};

pub const D: usize = 2;
pub type C = PoseidonGoldilocksConfig;
pub type F = <C as GenericConfig<D>>::F;

#[derive(Clone)]
pub struct Clock<const S: usize> {
    pub proof: ProofWithPublicInputs<F, C, D>,
}

impl<const S: usize> Debug for Clock<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let counters = self
            .counters()
            .enumerate()
            .filter(|(_, counter)| *counter != 0)
            .collect::<HashMap<_, _>>();
        write!(f, "Clock {:?}", counters)
    }
}

impl<const S: usize> Clock<S> {
    pub fn counters(&self) -> impl Iterator<Item = u32> + '_ {
        self.proof
            .public_inputs
            .iter()
            .take(S)
            .map(|counter| counter.to_canonical_u64() as _)
    }
}

#[derive(Debug)]
pub struct ClockCircuit<const S: usize> {
    pub data: CircuitData<F, C, D>,
    targets: Option<ClockCircuitTargets<S>>,
}

#[derive(Debug)]
struct ClockCircuitTargets<const S: usize> {
    // the only public input is the output clock, which is not expected to be set before proving
    // every target is witness

    // common inputs
    proof1: ProofWithPublicInputsTarget<D>,
    verifier_data1: VerifierCircuitTarget,

    // increment inputs, when merging...
    updated_index: Target,   // ...2^32
    updated_counter: Target, // ...F::NEG_ONE
    sig: Target,             // ...sign F::NEG_ONE with DUMMY_KEY

    // enable2: BoolTarget,
    // merge inputs, when incrementing...
    proof2: ProofWithPublicInputsTarget<D>, // ...same to `proof1`
    verifier_data2: VerifierCircuitTarget,  // ...same to `verifier_data1`
}

impl<const S: usize> ClockCircuit<S> {
    pub fn new_genesis(config: CircuitConfig) -> Self {
        let mut builder = CircuitBuilder::<F, D>::new(config);
        let output_counters = builder.constants(&[F::ZERO; S]);
        builder.register_public_inputs(&output_counters);
        Self {
            data: builder.build(),
            targets: None,
        }
    }

    pub fn new(
        inner: &Self,
        keys: &[HashOut<F>; S],
        dummy_key: HashOut<F>,
        config: CircuitConfig,
    ) -> Self {
        let mut builder = CircuitBuilder::<F, D>::new(config);

        let proof1 = builder.add_virtual_proof_with_pis(&inner.data.common);
        // the slicing is not necessary for now, just in case we add more public inputs later
        let input_counters1 = &proof1.public_inputs[..S];
        let proof2 = builder.add_virtual_proof_with_pis(&inner.data.common);
        let input_counters2 = &proof2.public_inputs[..S];

        let updated_index = builder.add_virtual_target();
        let updated_counter = builder.add_virtual_target();
        // although there is `add_virtual_nonnative_target`, but seems like currently it can only be
        // internally used by generators. there's no `set_nonnative_target` companion for witness
        // let num_limbs = CircuitBuilder::<F, D>::num_nonnative_limbs::<Secp256K1Scalar>();
        let sig = builder.add_virtual_target();

        let verifier_data1 =
            builder.add_virtual_verifier_data(inner.data.common.config.fri_config.cap_height);
        builder.verify_proof::<C>(&proof1, &verifier_data1, &inner.data.common);
        let verifier_data2 =
            builder.add_virtual_verifier_data(inner.data.common.config.fri_config.cap_height);
        builder.verify_proof::<C>(&proof2, &verifier_data2, &inner.data.common);
        // let enable2 = builder.add_virtual_bool_target_safe();
        // builder.conditionally_verify_proof_or_dummy::<C>(
        //     enable2,
        //     &proof2,
        //     &verifier_data2,
        //     &inner.data.common,
        // )?;

        let mut updated_key = builder.constant_hash(dummy_key);

        let output_counters = input_counters1
            .iter()
            .zip(input_counters2)
            .enumerate()
            .map(|(i, (input_counter1, input_counter2))| {
                let key = keys[i];
                let i = builder.constant(F::from_canonical_usize(i));
                let is_updated = builder.is_equal(updated_index, i);
                let x1 = U32Target(builder.select(is_updated, updated_counter, *input_counter1));

                let key = builder.constant_hash(key);
                let elements = updated_key
                    .elements
                    .iter()
                    .zip(&key.elements)
                    .map(|(updated_target, target)| {
                        builder.select(is_updated, *target, *updated_target)
                    })
                    .collect();
                updated_key = HashOutTarget::from_vec(elements);

                let x2 = U32Target(*input_counter2);
                range_check_u32_circuit(&mut builder, vec![x1, x2]);
                // max(x1, x2)
                let le = list_le_u32_circuit(&mut builder, vec![x1], vec![x2]);
                let U32Target(x1) = x1;
                let U32Target(x2) = x2;
                builder.select(le, x2, x1)
            })
            .collect::<Vec<_>>();

        // let msg = builder.biguint_to_nonnative::<Secp256K1Scalar>(&updated_counter);
        // verify_message_circuit(&mut builder, msg, sig, ECDSAPublicKeyTarget(updated_key));
        let key = builder.hash_n_to_hash_no_pad::<PoseidonHash>(vec![sig]);
        builder.connect_hashes(key, updated_key);

        builder.register_public_inputs(&output_counters);
        // builder.print_gate_counts(0);
        Self {
            data: builder.build(),
            targets: Some(ClockCircuitTargets {
                proof1,
                verifier_data1,
                proof2,
                verifier_data2,
                updated_index,
                updated_counter,
                sig,
                // enable2,
            }),
        }
    }

    pub fn with_data(data: CircuitData<F, C, D>, config: CircuitConfig) -> Self {
        Self {
            targets: Some(ClockCircuitTargets::new(&data, config)),
            data,
        }
    }
}

impl<const S: usize> ClockCircuitTargets<S> {
    fn new(circuit: &CircuitData<F, C, D>, config: CircuitConfig) -> Self {
        let mut builder = CircuitBuilder::new(config);
        // let num_limbs = CircuitBuilder::<F, D>::num_nonnative_limbs::<Secp256K1Scalar>();
        Self {
            proof1: builder.add_virtual_proof_with_pis(&circuit.common),
            proof2: builder.add_virtual_proof_with_pis(&circuit.common),
            updated_index: builder.add_virtual_target(),
            updated_counter: builder.add_virtual_target(),
            sig: builder.add_virtual_target(),
            verifier_data1: builder
                .add_virtual_verifier_data(circuit.common.config.fri_config.cap_height),
            verifier_data2: builder
                .add_virtual_verifier_data(circuit.common.config.fri_config.cap_height),
        }
    }
}

const DUMMY_SECRET: F = F::NEG_ONE;

impl<const S: usize> Clock<S> {
    pub fn genesis(
        keys: [HashOut<F>; S],
        config: CircuitConfig,
    ) -> anyhow::Result<(Self, ClockCircuit<S>)> {
        let mut circuit = ClockCircuit::new_genesis(config.clone());
        let mut timing =
            TimingTree::new("prove genesis", "INFO".parse().map_err(anyhow::Error::msg)?);
        let proof = prove(
            &circuit.data.prover_only,
            &circuit.data.common,
            PartialWitness::new(),
            &mut timing,
        )?;
        timing.print();
        let mut clock = Self {
            proof,
            // depth: 0
        };

        let dummy_key = public_key(DUMMY_SECRET);
        let mut inner_circuit = circuit;
        for _ in 0..4 {
            circuit = ClockCircuit::new(&inner_circuit, &keys, dummy_key, config.clone());
            clock = clock.merge_internal(&clock, &circuit, &inner_circuit)?;
            inner_circuit = circuit;
        }

        assert!(clock.counters().all(|counter| counter == 0));
        Ok((clock, inner_circuit))
    }

    pub fn with_proof_and_circuit(
        proof: ProofWithPublicInputs<F, C, D>,
        data: CircuitData<F, C, D>,
        config: CircuitConfig,
    ) -> (Self, ClockCircuit<S>) {
        (Self { proof }, ClockCircuit::with_data(data, config))
    }

    fn merge_internal(
        &self,
        other: &Self,
        circuit: &ClockCircuit<S>,
        inner_circuit: &ClockCircuit<S>,
    ) -> anyhow::Result<Self> {
        let clock1 = self;
        let clock2 = other;
        let mut pw = PartialWitness::new();
        let targets = circuit.targets.as_ref().unwrap();
        pw.set_proof_with_pis_target(&targets.proof1, &clock1.proof);
        pw.set_verifier_data_target(&targets.verifier_data1, &inner_circuit.data.verifier_only);
        pw.set_proof_with_pis_target(&targets.proof2, &clock2.proof);
        pw.set_verifier_data_target(&targets.verifier_data2, &inner_circuit.data.verifier_only);
        pw.set_target(targets.updated_index, F::from_canonical_usize(S + 1));
        pw.set_target(targets.updated_counter, F::from_canonical_u32(u32::MAX));
        // let msg = Secp256K1Scalar::from_canonical_u32(u32::MAX);
        // let sig = sign_message(msg, DUMMY_SECRET);
        pw.set_target(targets.sig, DUMMY_SECRET);

        let mut timing =
            TimingTree::new("prove merge", "INFO".parse().map_err(anyhow::Error::msg)?);
        let proof = prove(
            &circuit.data.prover_only,
            &circuit.data.common,
            pw,
            &mut timing,
        )?;
        timing.print();

        let clock = Self {
            proof,
            // depth: self.depth.max(other.depth),
        };
        assert!(clock
            .counters()
            .zip(clock1.counters())
            .zip(clock2.counters())
            .all(|((output_counter, input_counter1), input_counter2)| {
                output_counter == input_counter1.max(input_counter2)
            }));
        Ok(clock)
    }

    pub fn update(
        &self,
        index: usize,
        secret: F,
        other: &Self,
        circuit: &ClockCircuit<S>,
    ) -> anyhow::Result<Self> {
        let counter = self
            .counters()
            .nth(index)
            .ok_or(anyhow::anyhow!("out of bound index {index}"))?
            .max(
                other
                    .counters()
                    .nth(index)
                    .ok_or(anyhow::anyhow!("out of bound index {index}"))?,
            )
            + 1;
        let clock1 = self;
        let clock2 = other;
        let inner_circuit = circuit;
        let mut pw = PartialWitness::new();
        let targets = circuit.targets.as_ref().unwrap();
        pw.set_proof_with_pis_target(&targets.proof1, &clock1.proof);
        pw.set_verifier_data_target(&targets.verifier_data1, &inner_circuit.data.verifier_only);
        pw.set_proof_with_pis_target(&targets.proof2, &clock2.proof);
        pw.set_verifier_data_target(&targets.verifier_data2, &inner_circuit.data.verifier_only);
        pw.set_target(targets.updated_index, F::from_canonical_usize(index));
        pw.set_target(targets.updated_counter, F::from_canonical_u32(counter));
        // let msg = Secp256K1Scalar::from_canonical_u32(u32::MAX);
        // let sig = sign_message(msg, DUMMY_SECRET);
        pw.set_target(targets.sig, secret);

        let mut timing =
            TimingTree::new("prove update", "INFO".parse().map_err(anyhow::Error::msg)?);
        let proof = prove(
            &circuit.data.prover_only,
            &circuit.data.common,
            pw,
            &mut timing,
        )?;
        timing.print();

        let clock = Self {
            proof,
            // depth: self.depth.max(other.depth),
        };
        assert!(clock
            .counters()
            .enumerate()
            .zip(clock1.counters())
            .zip(clock2.counters())
            .all(|(((i, output_counter), input_counter1), input_counter2)| {
                if i == index {
                    output_counter == counter
                } else {
                    output_counter == input_counter1.max(input_counter2)
                }
            }));
        Ok(clock)
    }

    pub fn verify(&self, circuit: &ClockCircuit<S>) -> anyhow::Result<()> {
        circuit.data.verify(self.proof.clone()).map_err(Into::into)
    }
}

pub fn index_secret(index: usize) -> F {
    F::from_canonical_usize(117418 + index)
}

pub fn public_key(secret: F) -> HashOut<F> {
    hash_n_to_hash_no_pad::<_, PoseidonPermutation<_>>(&[secret])
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;

    const S: usize = 4;
    fn genesis_and_circuit() -> (Clock<S>, ClockCircuit<S>) {
        Clock::<S>::genesis(
            [(); S].map({
                let mut i = 0;
                move |()| {
                    let secret = index_secret(i);
                    i += 1;
                    public_key(secret)
                }
            }),
            CircuitConfig::standard_ecc_config(),
        )
        .unwrap()
    }

    static GENESIS_AND_CIRCUIT: OnceLock<(Clock<S>, ClockCircuit<S>)> = OnceLock::new();

    #[test]
    #[should_panic]
    fn malformed_signature() {
        let (genesis, circuit) = GENESIS_AND_CIRCUIT.get_or_init(genesis_and_circuit);
        genesis
            .update(0, index_secret(1), genesis, circuit)
            .unwrap();
    }

    #[test]
    #[should_panic]
    fn malformed_counters_recursive() {
        let (genesis, circuit) = GENESIS_AND_CIRCUIT.get_or_init(genesis_and_circuit);
        let clock1 = genesis.update(0, index_secret(0), genesis, circuit);
        let Ok(mut clock1) = clock1 else {
            return; // to trigger `should_panic` failure
        };
        clock1
            .proof
            .public_inputs
            .clone_from(&genesis.proof.public_inputs);
        clock1.update(0, index_secret(0), &clock1, circuit).unwrap();
    }
}
