use crate::{Bus, Event, Message, Permit, ReciveUnypedReceiver, TypeTag, error::Error, receiver::{
        Action, AnyReceiver, AnyWrapperRef, PermitDrop, ReceiverTrait, SendUntypedReceiver,
        TypeTagAccept,
    }, stats::Stats};
use dashmap::DashMap;
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
    receivers: DashMap<TypeTag, Arc<RelayReceiverContext>>,
    need_flush: AtomicBool,
    ready_flag: AtomicBool,
    init_sent: AtomicBool,
    flushed: Notify,
    synchronized: Notify,
    closed: Notify,
    ready: Notify,
}

pub struct RelayReceiverContext {
    limit: u64,
    processing: AtomicU64,
    response: Arc<Notify>,
}

impl RelayReceiverContext {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            processing: Default::default(),
            response: Arc::new(Notify::new()),
        }
    }
}

impl PermitDrop for RelayReceiverContext {
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
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            context: Arc::new(RelayContext {
                receivers: DashMap::new(),
                need_flush: AtomicBool::new(false),
                ready_flag: AtomicBool::new(false),
                init_sent: AtomicBool::new(false),
                flushed: Notify::new(),
                synchronized: Notify::new(),
                closed: Notify::new(),
                ready: Notify::new(),
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

    fn send_boxed(
        &self,
        mid: u64,
        boxed_msg: Box<dyn Message>,
        bus: &Bus,
    ) -> Result<(), Error<Box<dyn Message>>> {
        Ok(self.inner.send_msg(mid, boxed_msg, bus)?)
    }

    fn need_flush(&self) -> bool {
        self.context.need_flush.load(Ordering::SeqCst)
    }

    fn stats(&self) -> Stats {
        unimplemented!()
    }

    fn send_action(&self, bus: &Bus, action: Action) -> Result<(), Error<Action>> {
        Ok(SendUntypedReceiver::send(&self.inner, action, bus)?)
    }

    fn close_notify(&self) -> &Notify {
        &self.context.closed
    }

    fn sync_notify(&self) -> &Notify {
        &self.context.synchronized
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

    fn try_reserve(&self, tt: &TypeTag) -> Option<Permit> {
        if !self.context.receivers.contains_key(tt) {
            self.context
                .receivers
                .insert(tt.clone(), Arc::new(RelayReceiverContext::new(16)));
        }

        loop {
            let context = self.context.receivers.get(tt).unwrap();
            let count = context.processing.load(Ordering::Relaxed);

            if count < context.limit {
                let res = context.processing.compare_exchange(
                    count,
                    count + 1,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                if res.is_ok() {
                    break Some(Permit {
                        fuse: false,
                        inner: context.clone(),
                    });
                }

                // continue
            } else {
                break None;
            }
        }
    }

    fn reserve_notify(&self, tt: &TypeTag) -> Arc<Notify> {
        if !self.context.receivers.contains_key(tt) {
            self.context
                .receivers
                .insert(tt.clone(), Arc::new(RelayReceiverContext::new(16)));
        }

        self.context.receivers.get(tt).unwrap().response.clone()
    }

    fn ready_notify(&self) -> &Notify {
        &self.context.ready
    }

    fn is_ready(&self) -> bool {
        self.context.ready_flag.load(Ordering::SeqCst)
    }

    fn is_init_sent(&self) -> bool {
        self.context.init_sent.load(Ordering::SeqCst)
    }

    fn start_polling(
        self: Arc<Self>,
    ) -> Box<dyn FnOnce(Bus) -> Pin<Box<dyn Future<Output = ()> + Send>>> {
        Box::new(move |bus| {
            Box::pin(async move {
                loop {
                    let this = self.clone();
                    let bus = bus.clone();
                    let event = poll_fn(move |ctx| this.inner.poll_events(ctx, &bus)).await;

                    match event {
                        Event::Pause => self.context.ready_flag.store(false, Ordering::SeqCst),
                        Event::Ready => {
                            self.context.ready.notify_waiters();
                            self.context.ready_flag.store(true, Ordering::SeqCst)
                        }
                        Event::InitFailed(err) => {
                            error!("Relay init failed: {}", err);

                            self.context.ready.notify_waiters();
                            self.context.ready_flag.store(false, Ordering::SeqCst);
                        }
                        Event::Exited => {
                            self.context.closed.notify_waiters();
                            break;
                        }
                        Event::Flushed => self.context.flushed.notify_waiters(),
                        Event::Synchronized(_res) => self.context.synchronized.notify_waiters(),
                        Event::Response(mid, resp) => {
                            let tt = if let Ok(bm) = &resp {
                                Some(bm.type_tag())
                            } else {
                                None
                            };

                            if let Some(chan) = self.waiters.take(mid as _) {
                                if let Err(err) = chan.send(resp) {
                                    error!("Response error for mid({}): {:?}", mid, err);
                                }
                            } else {
                                warn!("No waiters for mid({})", mid);
                            };

                            if let Some(tt) = tt {
                                if let Some(ctx) = self.context.receivers.get(&tt) {
                                    ctx.processing.fetch_sub(1, Ordering::SeqCst);
                                    ctx.response.notify_one();
                                }
                            }
                        }

                        _ => unimplemented!(),
                    }
                }
            })
        })
    }
}
