use std::collections::HashMap;

use derive_where::derive_where;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::{
    cops::{self, DefaultVersion, DepOrd},
    crypto::{Crypto, Verifiable},
    event::{erased::OnEvent, OnTimer, SendEvent},
    lamport_mutex,
    net::{events::Recv, Addr, All, SendMessage},
    worker::Submit,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Announce<A> {
    prev: QuorumClock,
    merged: Vec<QuorumClock>,
    id: u64,
    addr: A,
}

#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub struct AnnounceOk {
    plain: DefaultVersion,
    id: u64,
    signer_id: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[derive_where(PartialOrd, PartialEq)]
pub struct QuorumClock {
    plain: DefaultVersion, // redundant, just for easier use
    #[derive_where(skip)]
    cert: Vec<Verifiable<AnnounceOk>>,
}

impl DepOrd for QuorumClock {
    fn dep_cmp(&self, other: &Self, id: crate::cops::KeyId) -> std::cmp::Ordering {
        self.plain.dep_cmp(&other.plain, id)
    }

    fn deps(&self) -> impl Iterator<Item = crate::cops::KeyId> + '_ {
        self.plain.deps()
    }
}

impl QuorumClock {
    pub fn verify(&self, num_faulty: usize, crypto: &Crypto) -> anyhow::Result<()> {
        if self.plain == DefaultVersion::default() {
            anyhow::ensure!(self.cert.is_empty()); // not necessary, just as sanity check
            return Ok(());
        }
        anyhow::ensure!(self.cert.len() > num_faulty);
        let indexes = self
            .cert
            .iter()
            .map(|verifiable| verifiable.signer_id)
            .collect::<Vec<_>>();
        crypto.verify_batched(&indexes, &self.cert)
    }
}

pub struct QuorumClient<U, N, A> {
    addr: A,
    num_faulty: usize,
    working_announces: HashMap<u64, WorkingAnnounce>,
    upcall: U,
    net: N,
}

struct WorkingAnnounce {
    prev_plain: DefaultVersion,
    replies: HashMap<usize, Verifiable<AnnounceOk>>,
}

impl<U, N, A> QuorumClient<U, N, A> {
    pub fn new(addr: A, num_faulty: usize, upcall: U, net: N) -> Self {
        Self {
            addr,
            num_faulty,
            upcall,
            net,
            working_announces: Default::default(),
        }
    }
}

struct SubmitAnnounce(QuorumClock, Vec<QuorumClock>, u64);

impl<U, N: SendMessage<All, Announce<A>>, A: Clone> OnEvent<SubmitAnnounce>
    for QuorumClient<U, N, A>
{
    fn on_event(
        &mut self,
        SubmitAnnounce(prev, merged, id): SubmitAnnounce,
        _: &mut impl crate::event::Timer,
    ) -> anyhow::Result<()> {
        let replaced = self.working_announces.insert(
            id,
            WorkingAnnounce {
                prev_plain: prev.plain.clone(),
                replies: Default::default(),
            },
        );
        anyhow::ensure!(replaced.is_none(), "concurrent announce on id {id}");
        let announce = Announce {
            prev,
            merged,
            id,
            addr: self.addr.clone(),
        };
        self.net.send(All, announce)
    }
}

// feel lazy to define event type for replying
impl<U: SendEvent<(u64, QuorumClock)>, N, A> OnEvent<Recv<Verifiable<AnnounceOk>>>
    for QuorumClient<U, N, A>
{
    fn on_event(
        &mut self,
        Recv(announce_ok): Recv<Verifiable<AnnounceOk>>,
        _: &mut impl crate::event::Timer,
    ) -> anyhow::Result<()> {
        let Some(working_state) = self.working_announces.get_mut(&announce_ok.id) else {
            return Ok(());
        };
        // sufficient rule out?
        if announce_ok
            .plain
            .dep_cmp(&working_state.prev_plain, announce_ok.id)
            .is_le()
        {
            return Ok(());
        }
        working_state
            .replies
            .insert(announce_ok.signer_id, announce_ok.clone());
        if working_state.replies.len() > self.num_faulty {
            let working_state = self.working_announces.remove(&announce_ok.id).unwrap();
            let announce_ok = announce_ok.into_inner();
            let clock = QuorumClock {
                plain: announce_ok.plain,
                cert: working_state.replies.into_values().collect(),
            };
            self.upcall.send((announce_ok.id, clock))?
        }
        Ok(())
    }
}

impl<U, N, A> OnTimer for QuorumClient<U, N, A> {
    fn on_timer(
        &mut self,
        _: crate::event::TimerId,
        _: &mut impl crate::event::Timer,
    ) -> anyhow::Result<()> {
        unreachable!()
    }
}

pub struct Lamport<E>(pub E, pub u64);

impl<E: SendEvent<SubmitAnnounce>> SendEvent<lamport_mutex::events::Update<QuorumClock>>
    for Lamport<E>
{
    fn send(&mut self, update: lamport_mutex::Update<QuorumClock>) -> anyhow::Result<()> {
        self.0
            .send(SubmitAnnounce(update.prev, vec![update.remote], self.1))
    }
}

impl<E: SendEvent<lamport_mutex::events::UpdateOk<QuorumClock>>> SendEvent<(u64, QuorumClock)>
    for Lamport<E>
{
    fn send(&mut self, (id, clock): (u64, QuorumClock)) -> anyhow::Result<()> {
        anyhow::ensure!(id == self.1);
        self.0.send(lamport_mutex::events::UpdateOk(clock))
    }
}

pub struct Cops<E>(pub E);

impl<E: SendEvent<SubmitAnnounce>> SendEvent<cops::events::Update<QuorumClock>> for Cops<E> {
    fn send(&mut self, update: cops::events::Update<QuorumClock>) -> anyhow::Result<()> {
        self.0
            .send(SubmitAnnounce(update.prev, update.deps, update.id))
    }
}

impl<E: SendEvent<cops::events::UpdateOk<QuorumClock>>> SendEvent<(u64, QuorumClock)> for Cops<E> {
    fn send(&mut self, (id, clock): (u64, QuorumClock)) -> anyhow::Result<()> {
        let update_ok = cops::events::UpdateOk {
            id,
            version_deps: clock,
        };
        self.0.send(update_ok)
    }
}

pub struct QuorumServer<CW, N> {
    id: usize,
    crypto_worker: CW,
    _m: std::marker::PhantomData<N>,
}

impl<CW, N> QuorumServer<CW, N> {
    pub fn new(id: usize, crypto_worker: CW) -> Self {
        Self {
            id,
            crypto_worker,
            _m: Default::default(),
        }
    }
}

impl<CW: Submit<Crypto, N>, N: SendMessage<A, Verifiable<AnnounceOk>>, A: Addr>
    OnEvent<Recv<Announce<A>>> for QuorumServer<CW, N>
{
    fn on_event(
        &mut self,
        Recv(announce): Recv<Announce<A>>,
        _: &mut impl crate::event::Timer,
    ) -> anyhow::Result<()> {
        let plain = announce.prev.plain.update(
            announce.merged.iter().map(|clock| &clock.plain),
            announce.id,
        );
        let announce_ok = AnnounceOk {
            plain,
            id: announce.id,
            signer_id: self.id,
        };
        debug!("signing {announce_ok:?}");
        self.crypto_worker.submit(Box::new(move |crypto, net| {
            net.send(announce.addr, crypto.sign(announce_ok))
        }))
    }
}

impl<CW, N> OnTimer for QuorumServer<CW, N> {
    fn on_timer(
        &mut self,
        _: crate::event::TimerId,
        _: &mut impl crate::event::Timer,
    ) -> anyhow::Result<()> {
        unreachable!()
    }
}

#[derive_where(Debug, Clone; CW)]
pub struct VerifyQuorumClock<CW, E> {
    num_faulty: usize,
    crypto_worker: CW,
    _m: std::marker::PhantomData<E>,
}

impl<CW, E> VerifyQuorumClock<CW, E> {
    pub fn new(num_faulty: usize, crypto_worker: CW) -> Self {
        Self {
            num_faulty,
            crypto_worker,
            _m: Default::default(),
        }
    }
}

trait VerifyClock: Send + Sync + 'static {
    fn verify_clock(&self, num_faulty: usize, crypto: &Crypto) -> anyhow::Result<()>;
}

impl<CW: Submit<Crypto, E>, E: SendEvent<Recv<M>>, M: VerifyClock> SendEvent<Recv<M>>
    for VerifyQuorumClock<CW, E>
{
    fn send(&mut self, Recv(message): Recv<M>) -> anyhow::Result<()> {
        let num_faulty = self.num_faulty;
        self.crypto_worker.submit(Box::new(move |crypto, sender| {
            if message.verify_clock(num_faulty, crypto).is_ok() {
                sender.send(Recv(message))
            } else {
                warn!("clock verification failed");
                Ok(())
            }
        }))
    }
}

impl<A: Addr> VerifyClock for Announce<A> {
    fn verify_clock(&self, num_faulty: usize, crypto: &Crypto) -> anyhow::Result<()> {
        self.prev.verify(num_faulty, crypto)?;
        for clock in &self.merged {
            clock.verify(num_faulty, crypto)?
        }
        Ok(())
    }
}

impl<M: Send + Sync + 'static> VerifyClock for lamport_mutex::Clocked<M, QuorumClock> {
    fn verify_clock(&self, num_faulty: usize, crypto: &Crypto) -> anyhow::Result<()> {
        self.clock.verify(num_faulty, crypto)
    }
}

impl VerifyClock for cops::PutOk<QuorumClock> {
    fn verify_clock(&self, num_faulty: usize, crypto: &Crypto) -> anyhow::Result<()> {
        self.version_deps.verify(num_faulty, crypto)
    }
}

impl VerifyClock for cops::GetOk<QuorumClock> {
    fn verify_clock(&self, num_faulty: usize, crypto: &Crypto) -> anyhow::Result<()> {
        self.version_deps.verify(num_faulty, crypto)
    }
}

impl<A: Addr> VerifyClock for cops::Put<QuorumClock, A> {
    fn verify_clock(&self, num_faulty: usize, crypto: &Crypto) -> anyhow::Result<()> {
        for clock in self.deps.values() {
            clock.verify(num_faulty, crypto)?
        }
        Ok(())
    }
}

impl<A: Addr> VerifyClock for cops::Get<A> {
    fn verify_clock(&self, _: usize, _: &Crypto) -> anyhow::Result<()> {
        Ok(())
    }
}

impl VerifyClock for cops::SyncKey<QuorumClock> {
    fn verify_clock(&self, num_faulty: usize, crypto: &Crypto) -> anyhow::Result<()> {
        self.version_deps.verify(num_faulty, crypto)
    }
}

// cSpell:words lamport upcall
