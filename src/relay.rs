use crate::{
    error::Error,
    receiver::{
        Action, AnyReceiver, AnyWrapperArc, AnyWrapperRef, PermitDrop, ReceiverTrait,
        SendUntypedReceiver, Stats, TypeTagAccept,
    },
    Bus, Event, Message, Permit, ReciveUnypedReceiver, TypeTag,
};
use futures::{future::poll_fn, Future};
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::{oneshot, Notify};

pub trait Relay: TypeTagAccept + SendUntypedReceiver + ReciveUnypedReceiver + 'static {}
impl<T: TypeTagAccept + SendUntypedReceiver + ReciveUnypedReceiver + 'static> Relay for T {}

struct SlabCfg;
impl sharded_slab::Config for SlabCfg {
    const RESERVED_BITS: usize = 1;
}

type Slab<T> = sharded_slab::Slab<T, SlabCfg>;

pub(crate) struct RelayContext {
    limit: u64,
    processing: AtomicU64,
    need_flush: AtomicBool,
    flushed: Notify,
    synchronized: Notify,
    closed: Notify,
    response: Notify,
}

impl PermitDrop for RelayContext {
    fn permit_drop(&self) {
        self.processing.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) struct RelayWrapper<S>
where
    S: 'static,
{
    inner: S,
    context: Arc<RelayContext>,
    waiters: Slab<oneshot::Sender<Result<Box<dyn Message>, Error>>>,
}
impl<S> RelayWrapper<S> {
    pub fn new(inner: S, limit: u64) -> Self {
        Self {
            inner,
            context: Arc::new(RelayContext {
                limit,
                processing: AtomicU64::new(0),
                need_flush: AtomicBool::new(false),
                flushed: Notify::new(),
                synchronized: Notify::new(),
                closed: Notify::new(),
                response: Notify::new(),
            }),
            waiters: sharded_slab::Slab::new_with_config::<SlabCfg>(),
        }
    }
}

impl<S> TypeTagAccept for RelayWrapper<S>
where
    S: Relay + Send + Sync + 'static,
{
    fn iter_types(&self, cb: &mut dyn FnMut(&TypeTag, &TypeTag, &TypeTag) -> bool) {
        self.inner.iter_types(cb)
    }

    fn accept(&self, msg: &TypeTag, resp: Option<&TypeTag>, err: Option<&TypeTag>) -> bool {
        self.inner.accept(msg, resp, err)
    }
}

impl<S> ReceiverTrait for RelayWrapper<S>
where
    S: Relay + Send + Sync + 'static,
{
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }

    fn typed(&self) -> Option<AnyReceiver<'_>> {
        None
    }
    fn wrapper(&self) -> Option<AnyWrapperRef<'_>> {
        None
    }
    fn wrapper_arc(self: Arc<Self>) -> Option<AnyWrapperArc> {
        None
    }
    fn send_boxed(
        &self,
        mid: u64,
        boxed_msg: Box<dyn Message>,
    ) -> Result<(), Error<Box<dyn Message>>> {
        Ok(self.inner.send_msg(mid, boxed_msg)?)
    }

    fn need_flush(&self) -> bool {
        self.context.need_flush.load(Ordering::SeqCst)
    }

    fn stats(&self) -> Result<Stats, Error<Action>> {
        unimplemented!()
    }

    fn close(&self) -> Result<(), Error<Action>> {
        Ok(SendUntypedReceiver::send(&self.inner, Action::Close)?)
    }

    fn close_notify(&self) -> &Notify {
        &self.context.closed
    }

    fn sync(&self) -> Result<(), Error<Action>> {
        Ok(SendUntypedReceiver::send(&self.inner, Action::Sync)?)
    }

    fn sync_notify(&self) -> &Notify {
        &self.context.synchronized
    }

    fn flush(&self) -> Result<(), Error<Action>> {
        Ok(SendUntypedReceiver::send(&self.inner, Action::Flush)?)
    }

    fn flush_notify(&self) -> &Notify {
        &self.context.flushed
    }

    fn add_response_listener(
        &self,
        listener: oneshot::Sender<Result<Box<dyn Message>, Error>>,
    ) -> Result<u64, Error> {
        Ok(self
            .waiters
            .insert(listener)
            .ok_or_else(|| Error::AddListenerError)? as _)
    }

    fn try_reserve(&self) -> Option<Permit> {
        loop {
            let count = self.context.processing.load(Ordering::Relaxed);

            if count < self.context.limit {
                let res = self.context.processing.compare_exchange(
                    count,
                    count + 1,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                if res.is_ok() {
                    break Some(Permit {
                        fuse: false,
                        inner: self.context.clone(),
                    });
                }

                // continue
            } else {
                break None;
            }
        }
    }

    fn reserve_notify(&self) -> &Notify {
        &self.context.response
    }

    fn start_polling(
        self: Arc<Self>,
    ) -> Box<dyn FnOnce(Bus) -> Pin<Box<dyn Future<Output = ()> + Send>>> {
        Box::new(move |_| {
            Box::pin(async move {
                loop {
                    let this = self.clone();
                    let event = poll_fn(move |ctx| this.inner.poll_events(ctx)).await;

                    match event {
                        Event::Exited => {
                            self.context.closed.notify_waiters();
                            break;
                        }
                        Event::Flushed => self.context.flushed.notify_waiters(),
                        Event::Synchronized(_res) => self.context.synchronized.notify_waiters(),
                        Event::Response(mid, resp) => {
                            self.context.processing.fetch_sub(1, Ordering::SeqCst);
                            self.context.response.notify_one();

                            if let Some(chan) = self.waiters.take(mid as _) {
                                if let Err(err) = chan.send(resp) {
                                    error!("Response error for mid({}): {:?}", mid, err);
                                }
                            } else {
                                warn!("No waiters for mid({})", mid);
                            }
                        }

                        _ => unimplemented!(),
                    }
                }
            })
        })
    }
}
