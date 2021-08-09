mod builder;
mod envelop;
pub mod error;
mod handler;
mod receiver;
pub mod receivers;
mod relay;
pub mod relays;
mod trait_object;

#[macro_use]
extern crate log;

pub mod derive {
    pub use messagebus_derive::*;
}

use crate::receiver::Permit;
pub use builder::Module;
use builder::{BusBuilder, MessageTypeDescriptor};
use core::any::Any;
pub use envelop::{IntoBoxedMessage, Message, MessageBounds, SharedMessage, TypeTag, TypeTagged};
use error::{Error, SendError, StdSyncSendError};
pub use handler::*;
use receiver::Receiver;
pub use receiver::{
    Action, Event, ReciveTypedReceiver, ReciveUnypedReceiver, SendTypedReceiver,
    SendUntypedReceiver, TypeTagAccept,
};
pub use relay::Relay;
use smallvec::SmallVec;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

pub type Untyped = Arc<dyn Any + Send + Sync>;

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SendOptions {
    Broadcast,
    Direct(u64),
    Random,
    Balanced,
}

impl Default for SendOptions {
    fn default() -> Self {
        Self::Broadcast
    }
}

pub struct BusInner {
    message_types: HashMap<TypeTag, MessageTypeDescriptor>,
    receivers: HashMap<TypeTag, SmallVec<[Receiver; 4]>>,
    closed: AtomicBool,
}

impl BusInner {
    pub(crate) fn new(
        receivers: HashMap<TypeTag, SmallVec<[Receiver; 4]>>,
        message_types: HashMap<TypeTag, MessageTypeDescriptor>,
    ) -> Self {
        Self {
            message_types,
            receivers,
            closed: AtomicBool::new(false),
        }
    }
}

#[derive(Clone)]
pub struct Bus {
    inner: Arc<BusInner>,
}

impl Bus {
    #[inline]
    pub fn build() -> BusBuilder {
        BusBuilder::new()
    }

    pub(crate) fn init(&self) {
        for (_, rs) in &self.inner.receivers {
            for r in rs {
                println!("init {}", r.name());
                r.init(self).unwrap();
            }
        }
    }

    pub async fn ready(&self) {
        for (_, rs) in &self.inner.receivers {
            for r in rs {
                r.ready().await;
            }
        }
    }

    pub async fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);

        for (_, rs) in &self.inner.receivers {
            for r in rs {
                r.close(self).await;
            }
        }
    }

    pub async fn flush(&self) {
        let fuse_count = 32i32;
        let mut breaked = false;
        let mut iters = 0usize;
        for _ in 0..fuse_count {
            iters += 1;
            let mut flushed = false;
            for (_, rs) in &self.inner.receivers {
                for r in rs {
                    if r.need_flush() {
                        flushed = true;
                        r.flush(self).await;
                    }
                }
            }

            if !flushed {
                breaked = true;
                break;
            }
        }

        if !breaked {
            warn!(
                "!!! WARNING: unable to reach equilibrium in {} iterations !!!",
                fuse_count
            );
        } else {
            info!("flushed in {} iterations !!!", iters);
        }
    }

    pub async fn flush_and_sync(&self) {
        self.flush().await;

        for (_, rs) in &self.inner.receivers {
            for r in rs {
                r.sync(self).await;
            }
        }
    }

    fn try_reserve(&self, tt: &TypeTag, rs: &[Receiver]) -> Option<SmallVec<[Permit; 32]>> {
        let mut permits = SmallVec::<[Permit; 32]>::new();

        for r in rs {
            if let Some(prmt) = r.try_reserve(tt) {
                permits.push(prmt);
            } else {
                return None;
            };
        }

        Some(permits)
    }

    #[inline]
    pub fn try_send<M: Message + Clone>(&self, msg: M) -> Result<(), Error<M>> {
        self.try_send_ext(msg, SendOptions::Broadcast)
    }

    pub fn try_send_ext<M: Message + Clone>(
        &self,
        msg: M,
        _options: SendOptions,
    ) -> core::result::Result<(), Error<M>> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(msg).into());
        }

        let tt = msg.type_tag();
        let mid = ID_COUNTER.fetch_add(1, Ordering::Relaxed);

        if let Some(rs) = self.inner.receivers.get(&tt) {
            let permits = if let Some(x) = self.try_reserve(&tt, rs) {
                x
            } else {
                return Err(SendError::Full(msg).into());
            };

            let mut iter = permits.into_iter().zip(rs.iter());
            let mut counter = 1;
            let total = rs.len();

            while counter < total {
                let (p, r) = iter.next().unwrap();
                let _ = r.send(self, mid, msg.clone(), p);

                counter += 1;
            }

            if let Some((p, r)) = iter.next() {
                let _ = r.send(self, mid, msg, p);
                return Ok(());
            }
        }

        warn!(
            "Unhandled message {:?}: no receivers",
            core::any::type_name::<M>()
        );

        Ok(())
    }

    #[inline]
    pub fn send_blocking<M: Message + Clone>(&self, msg: M) -> Result<(), Error<M>> {
        self.send_blocking_ext(msg, SendOptions::Broadcast)
    }

    #[inline]
    pub fn send_blocking_ext<M: Message + Clone>(
        &self,
        msg: M,
        options: SendOptions,
    ) -> core::result::Result<(), Error<M>> {
        futures::executor::block_on(self.send_ext(msg, options))
    }

    #[inline]
    pub async fn send<M: Message + Clone>(&self, msg: M) -> core::result::Result<(), Error<M>> {
        Ok(self.send_ext(msg, SendOptions::Broadcast).await?)
    }

    pub async fn send_ext<M: Message + Clone>(
        &self,
        msg: M,
        _options: SendOptions,
    ) -> core::result::Result<(), Error<M>> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(msg).into());
        }

        for r in &self.inner.receivers {
            println!("{:?}: ", r.0);
            for i in r.1 {
                println!("  {:?}: ", i.name());
            }
        }

        let tt = msg.type_tag();
        let mid = ID_COUNTER.fetch_add(1, Ordering::Relaxed);

        if let Some(rs) = self.inner.receivers.get(&tt) {
            if let Some((last, head)) = rs.split_last() {
                for r in head {
                    let _ = r.send(self, mid, msg.clone(), r.reserve(&tt).await);
                }

                let _ = last.send(self, mid, msg, last.reserve(&tt).await);

                return Ok(());
            }
        }

        warn!(
            "Unhandled message {:?}: no receivers",
            core::any::type_name::<M>()
        );

        Ok(())
    }

    #[inline]
    pub fn force_send<M: Message + Clone>(&self, msg: M) -> Result<(), Error<M>> {
        self.force_send_ext(msg, SendOptions::Broadcast)
    }

    pub fn force_send_ext<M: Message + Clone>(
        &self,
        msg: M,
        _options: SendOptions,
    ) -> core::result::Result<(), Error<M>> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(msg).into());
        }

        let tt = msg.type_tag();
        let mid = ID_COUNTER.fetch_add(1, Ordering::Relaxed);

        if let Some(rs) = self.inner.receivers.get(&tt) {
            if let Some((last, head)) = rs.split_last() {
                for r in head {
                    let _ = r.force_send(self, mid, msg.clone());
                }

                let _ = last.force_send(self, mid, msg);

                return Ok(());
            }
        }

        warn!(
            "Unhandled message {:?}: no receivers",
            core::any::type_name::<M>()
        );

        Ok(())
    }

    #[inline]
    pub fn try_send_one<M: Message>(&self, msg: M) -> Result<(), Error<M>> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(msg).into());
        }

        let tt = msg.type_tag();
        let mid = ID_COUNTER.fetch_add(1, Ordering::Relaxed);

        if let Some(rs) = self.inner.receivers.get(&tt).and_then(|rs| rs.first()) {
            let permits = if let Some(x) = rs.try_reserve(&tt) {
                x
            } else {
                return Err(SendError::Full(msg).into());
            };

            Ok(rs.send(self, mid, msg, permits)?)
        } else {
            Err(Error::NoReceivers)
        }
    }

    pub async fn send_one<M: Message>(&self, msg: M) -> Result<(), Error<M>> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(msg).into());
        }

        let tt = msg.type_tag();
        let mid = ID_COUNTER.fetch_add(1, Ordering::Relaxed);

        if let Some(rs) = self.inner.receivers.get(&tt).and_then(|rs| rs.first()) {
            Ok(rs.send(self, mid, msg, rs.reserve(&tt).await)?)
        } else {
            Err(Error::NoReceivers)
        }
    }

    #[inline]
    pub fn send_one_blocking<M: Message>(&self, msg: M) -> Result<(), Error<M>> {
        futures::executor::block_on(self.send_one(msg))
    }

    pub async fn request<M: Message, R: Message>(
        &self,
        req: M,
        options: SendOptions,
    ) -> Result<R, Error<M>> {
        let tid = req.type_tag();
        let rid = R::type_tag_();

        let mut iter = self.select_receivers(&tid, options, Some(&rid), None);
        if let Some(rc) = iter.next() {
            let (mid, rx) = rc
                .add_response_waiter::<R>()
                .map_err(|x| x.specify::<M>())?;

            let mid = mid | 1 << (u64::BITS - 1);

            rc.send(self, mid, req, rc.reserve(&tid).await)?;
            rx.await.map_err(|x| x.specify::<M>())
        } else {
            Err(Error::NoReceivers)
        }
    }

    pub async fn request_we<M, R, E>(&self, req: M, options: SendOptions) -> Result<R, Error<M, E>>
    where
        M: Message,
        R: Message,
        E: StdSyncSendError,
    {
        let tid = M::type_tag_();
        let rid = R::type_tag_();
        let eid = E::type_tag_();

        let mut iter = self.select_receivers(&tid, options, Some(&rid), Some(&eid));
        if let Some(rc) = iter.next() {
            let (mid, rx) = rc.add_response_waiter_we::<R, E>().map_err(|x| {
                x.map_err(|_| unimplemented!())
                    .map_msg(|_| unimplemented!())
            })?;

            rc.send(
                self,
                mid | 1 << (u64::BITS - 1),
                req,
                rc.reserve(&tid).await,
            )
            .map_err(|x| x.map_err(|_| unimplemented!()))?;

            rx.await.map_err(|x| x.specify::<M>())
        } else {
            Err(Error::NoReceivers)
        }
    }

    pub async fn send_boxed(
        &self,
        msg: Box<dyn Message>,
        _options: SendOptions,
    ) -> Result<(), Error<Box<dyn Message>>> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(msg).into());
        }

        let tt = msg.type_tag();
        let mid = ID_COUNTER.fetch_add(1, Ordering::Relaxed);

        if let Some(rs) = self.inner.receivers.get(&tt) {
            if let Some((last, head)) = rs.split_last() {
                for r in head {
                    let _ = r.send_boxed(
                        self,
                        mid,
                        msg.try_clone_boxed().unwrap(),
                        r.reserve(&tt).await,
                    );
                }

                let _ = last.send_boxed(self, mid, msg, last.reserve(&tt).await);

                return Ok(());
            }
        }

        warn!("Unhandled message: no receivers");

        Ok(())
    }

    pub async fn send_boxed_one(
        &self,
        msg: Box<dyn Message>,
        _options: SendOptions,
    ) -> Result<(), Error<Box<dyn Message>>> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(msg).into());
        }

        let tt = msg.type_tag();
        let mid = ID_COUNTER.fetch_add(1, Ordering::Relaxed);

        if let Some(rs) = self.inner.receivers.get(&tt).and_then(|rs| rs.first()) {
            Ok(rs.send_boxed(self, mid, msg, rs.reserve(&tt).await)?)
        } else {
            Err(Error::NoReceivers)
        }
    }

    pub async fn request_boxed(
        &self,
        req: Box<dyn Message>,
        options: SendOptions,
    ) -> Result<Box<dyn Message>, Error<Box<dyn Message>>> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(req).into());
        }

        let tt = req.type_tag();

        let mut iter = self.select_receivers(&tt, options, None, None);
        if let Some(rc) = iter.next() {
            let (mid, rx) = rc.add_response_waiter_boxed().map_err(|x| {
                x.map_err(|_| unimplemented!())
                    .map_msg(|_| unimplemented!())
            })?;

            rc.send_boxed(
                self,
                mid | 1 << (usize::BITS - 1),
                req,
                rc.reserve(&tt).await,
            )?;

            rx.await.map_err(|x| x.specify::<Box<dyn Message>>())
        } else {
            Err(Error::NoReceivers)
        }
    }

    pub async fn send_deserialize_one<'a, 'b: 'a, 'c: 'a>(
        &'a self,
        tt: TypeTag,
        de: &'b mut dyn erased_serde::Deserializer<'c>,
        _options: SendOptions,
    ) -> Result<(), Error<Box<dyn Message>>> {
        if self.inner.closed.load(Ordering::SeqCst) {
            println!("closed message bus");
            return Err(Error::NoResponse);
        }

        let mid = ID_COUNTER.fetch_add(1, Ordering::Relaxed);

        if let Some(rs) = self.inner.receivers.get(&tt).and_then(|rs| rs.first()) {
            let msg = self.deserialize_message(tt.clone(), de)?;
            Ok(rs.send_boxed(self, mid, msg, rs.reserve(&tt).await)?)
        } else {
            Err(Error::NoReceivers)
        }
    }

    pub async fn request_deserialize<'a, 'b: 'a, 'c: 'a>(
        &'a self,
        tt: TypeTag,
        de: &'b mut dyn erased_serde::Deserializer<'c>,
        options: SendOptions,
    ) -> Result<Box<dyn Message>, Error<Box<dyn Message>>> {
        if self.inner.closed.load(Ordering::SeqCst) {
            println!("closed message bus");
            return Err(Error::NoResponse);
        }

        let mut iter = self.select_receivers(&tt, options, None, None);
        if let Some(rc) = iter.next() {
            let (mid, rx) = rc.add_response_waiter_boxed().unwrap();
            let msg = self.deserialize_message(tt.clone(), de)?;

            rc.send_boxed(
                self,
                mid | 1 << (usize::BITS - 1),
                msg,
                rc.reserve(&tt).await,
            )?;

            rx.await.map_err(|x| x.specify::<Box<dyn Message>>())
        } else {
            Err(Error::NoReceivers)
        }
    }

    pub fn deserialize_message(
        &self,
        tt: TypeTag,
        de: &mut dyn erased_serde::Deserializer<'_>,
    ) -> Result<Box<dyn Message>, Error<Box<dyn Message>>> {
        let md = self
            .inner
            .message_types
            .get(&tt)
            .ok_or_else(|| Error::TypeTagNotRegistered(tt))?;

        md.deserialize_boxed(de)
            .map_err(|err| err.specify::<Box<dyn Message>>())
    }

    #[inline]
    fn select_receivers<'a, 'b: 'a, 'c: 'a, 'd: 'a>(
        &'a self,
        tid: &'b TypeTag,
        _options: SendOptions,
        rid: Option<&'c TypeTag>,
        eid: Option<&'d TypeTag>,
    ) -> impl Iterator<Item = &Receiver> + 'a {
        self.inner
            .receivers
            .get(tid)
            .into_iter()
            .map(|item| item.iter())
            .flatten()
            .filter(move |x| x.accept(tid, rid, eid))
    }
}
