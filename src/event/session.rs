use std::{collections::HashMap, fmt::Debug, time::Duration};

use derive_where::derive_where;
use tokio::{
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    task::{AbortHandle, JoinError, JoinSet},
    time::{interval, sleep},
};

use crate::event::{SendEvent, Timer, TimerId};

use super::{OnEventUniversal, OnTimerUniversal, SendEventOnce};

// useful impl on foreign types that has been also leveraged elsewhere
// consider move to more proper place

#[derive_where(Debug)]
#[derive(derive_more::Display, derive_more::Error)]
#[display(fmt = "{}", display)]
pub struct SendError<M> {
    display: String,
    #[derive_where(skip)]
    pub inner: M,
}
// this suppose to be the error type of `UnboundedSender<_> as SendEvent<_>` and
// `oneshot::Sender<_> as SendEvent<_>`, but currently it is not
// if i want to turn (i.e. wrap) it into a `anyhow::Error`, it must be
// `Send + Sync + 'static`, which means `M` must be `Send + Sync + 'static`
// this is stricter than the minimal trait bound to make the two senders usable,
// which is just `Send`
// (details: the senders are `Send + Sync` as long as `M` is `Send`. senders'
// lifetime is bounded by `M`'s lifetime, and not necessary to be `'static` as
// long as you are ok with a sender that can only lives shorter)
// although `M` is probably already `Send + 'static` through this codebase, they
// may not be `Sync`, most notably, `event::erased::Event<...>` and
// `worker::Work<...>`
// sure they can be `Sync` at any time, since they are under my control and are
// already be `Send` for working with this event loop and be `'static` just for
// simplicity. but i don't want to add one hundred `+ Sync` for this rarely used
// feature that allows you to retrieve sent item that failed to be sent
// the correct approach and the ideal solution for this issue is that what i
// actually desired through this codebase is `Box<dyn Error + Send>`, as i
// probably never want to share an error across threads. (should this be a
// common practice? i start to wonder why anyhow decides errors to must be
// `Sync` at the first place)
// but back to the real world, anyhow will probably never add a `Send` only
// variant, and vanilla `Box<dyn std::error::Error + Send>` will still take long
// to be a drop in replacement of anyhow, so all i can do for now is noting down
// this approach and start to make tradeoffs

impl<N: Into<M>, M> SendEvent<N> for UnboundedSender<M> {
    fn send(&mut self, event: N) -> anyhow::Result<()> {
        UnboundedSender::send(self, event.into())
            .map_err(|err| anyhow::format_err!(err.to_string()))
    }
}

impl<N: Into<M>, M> SendEventOnce<N> for oneshot::Sender<M> {
    fn send_once(self, event: N) -> anyhow::Result<()> {
        self.send(event.into())
            .map_err(|_| anyhow::format_err!("send once failed"))
    }
}

#[derive(Debug)]
enum Event<M> {
    Timer(u32),
    Other(M),
}

#[derive(Debug)]
pub struct Sender<M>(UnboundedSender<Event<M>>);

impl<M> Clone for Sender<M> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<N: Into<M>, M> SendEvent<N> for Sender<M> {
    fn send(&mut self, event: N) -> anyhow::Result<()> {
        SendEvent::send(&mut self.0, Event::Other(event.into()))
    }
}

#[derive(Debug)]
pub struct Session<M> {
    sender: UnboundedSender<Event<M>>,
    receiver: UnboundedReceiver<Event<M>>,
    timer: SessionTimer,
}

trait SendTimerId {
    fn send(&mut self, timer_id: u32) -> anyhow::Result<()>;

    fn boxed_clone(&self) -> Box<dyn SendTimerId + Send + Sync>;
}

impl<M: Send + 'static> SendTimerId for UnboundedSender<Event<M>> {
    fn send(&mut self, timer_id: u32) -> anyhow::Result<()> {
        SendEvent::send(self, Event::Timer(timer_id))
    }

    fn boxed_clone(&self) -> Box<dyn SendTimerId + Send + Sync> {
        Box::new(self.clone())
    }
}

pub struct SessionTimer {
    sender: Box<dyn SendTimerId + Send + Sync>,
    id: u32,
    sessions: JoinSet<anyhow::Result<()>>,
    handles: HashMap<u32, AbortHandle>,
}

impl Debug for SessionTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTimer")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl<M: Send + 'static> Session<M> {
    pub fn new() -> Self {
        let (sender, receiver) = unbounded_channel();
        Self {
            sender: sender.clone(),
            receiver,
            timer: SessionTimer {
                sender: Box::new(sender),
                id: 0,
                sessions: Default::default(),
                handles: Default::default(),
            },
        }
    }
}

impl<M: Send + 'static> Default for Session<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> Session<M> {
    pub fn sender(&self) -> Sender<M> {
        Sender(self.sender.clone())
    }

    pub async fn run(
        &mut self,
        state: &mut (impl OnEventUniversal<SessionTimer, Event = M> + OnTimerUniversal<SessionTimer>),
    ) -> anyhow::Result<()>
    where
        M: Send + 'static,
    {
        loop {
            enum Select<M> {
                JoinNext(Result<anyhow::Result<()>, JoinError>),
                Recv(Option<Event<M>>),
            }
            let event = match tokio::select! {
                Some(result) = self.timer.sessions.join_next() => Select::JoinNext(result),
                recv = self.receiver.recv() => Select::Recv(recv)
            } {
                Select::JoinNext(Err(err)) if err.is_cancelled() => continue,
                Select::JoinNext(result) => {
                    result??;
                    continue;
                }
                Select::Recv(event) => event.ok_or(anyhow::format_err!("channel closed"))?,
            };
            match event {
                Event::Timer(timer_id) => {
                    if !self.timer.handles.contains_key(&timer_id) {
                        // unset/timeout contention, force to skip timer as long as it has been
                        // unset
                        // this could happen because of stalled timers in event waiting list
                        // another approach has been taken previously, by passing the timer events
                        // with a shared mutex state `timeouts`
                        // that should (probably) avoid this case in a single-thread runtime, but
                        // since tokio does not offer a generally synchronous `abort`, the following
                        // sequence is still possible in multithreading runtime
                        //   event loop lock `timeouts`
                        //   event callback `unset` timer which calls `abort`
                        //   event callback returns, event loop unlock `timeouts`
                        //   timer coroutine keep alive, lock `timeouts` and push event into it
                        //   timer coroutine finally get aborted
                        // the (probably) only solution is to implement a synchronous abort, block
                        // in `unset` call until timer coroutine replies with somehow promise of not
                        // sending timer event anymore, i don't feel that worth
                        // anyway, as long as this fallback presents the `abort` is logically
                        // redundant, just for hopefully better performance
                        // (so wish i have direct access to the timer wheel...)
                        continue;
                    }
                    state.on_timer(TimerId(timer_id), &mut self.timer)?
                }
                Event::Other(event) => state.on_event(event, &mut self.timer)?,
            }
        }
    }
}

impl Timer for SessionTimer {
    fn set(&mut self, period: Duration) -> anyhow::Result<TimerId> {
        let period = period.max(Duration::from_nanos(1));
        self.id += 1;
        let timer_id = self.id;
        let mut sender = self.sender.boxed_clone();
        let handle = self.sessions.spawn(async move {
            sleep(period).await;
            let mut interval = interval(period);
            loop {
                interval.tick().await;
                sender.send(timer_id)?
            }
        });
        self.handles.insert(timer_id, handle);
        Ok(TimerId(timer_id))
    }

    fn unset(&mut self, TimerId(timer_id): TimerId) -> anyhow::Result<()> {
        self.handles
            .remove(&timer_id)
            .ok_or(anyhow::format_err!("timer not exists"))?
            .abort();
        Ok(())
    }
}
