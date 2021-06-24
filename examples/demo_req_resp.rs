use async_trait::async_trait;
use messagebus::{error::Error, receivers, AsyncHandler, Bus};

struct TmpReceiver1;
struct TmpReceiver2;

#[async_trait]
impl AsyncHandler<i32> for TmpReceiver1 {
    type Error = Error;
    type Response = f32;

    async fn handle(&self, msg: i32, bus: &Bus) -> Result<Self::Response, Self::Error> {
        let resp1 = bus.request::<_, f32>(10i16, Default::default()).await?;
        let resp2 = bus.request::<_, f32>(20u16, Default::default()).await?;

        Ok(msg as f32 + resp1 + resp2)
    }

    async fn sync(&self, _bus: &Bus) -> Result<(), Self::Error> {
        println!("TmpReceiver1 i32: sync");

        Ok(())
    }
}

#[async_trait]
impl AsyncHandler<u32> for TmpReceiver1 {
    type Error = Error;
    type Response = f32;

    async fn handle(&self, msg: u32, _bus: &Bus) -> Result<Self::Response, Self::Error> {
        Ok(msg as f32)
    }
    async fn sync(&self, _bus: &Bus) -> Result<(), Self::Error> {
        println!("TmpReceiver1 u32: sync");

        Ok(())
    }
}

#[async_trait]
impl AsyncHandler<i16> for TmpReceiver1 {
    type Error = Error;
    type Response = f32;

    async fn handle(&self, msg: i16, bus: &Bus) -> Result<Self::Response, Self::Error> {
        let resp1 = bus.request::<_, f32>(1i8, Default::default()).await?;
        let resp2 = bus.request::<_, f32>(2u8, Default::default()).await?;

        Ok(msg as f32 + resp1 + resp2)
    }

    async fn sync(&self, _bus: &Bus) -> Result<(), Self::Error> {
        println!("TmpReceiver i16: sync");

        Ok(())
    }
}

#[async_trait]
impl AsyncHandler<u16> for TmpReceiver1 {
    type Error = Error;
    type Response = f32;

    async fn handle(&self, msg: u16, _bus: &Bus) -> Result<Self::Response, Self::Error> {
        Ok(msg as f32)
    }

    async fn sync(&self, _bus: &Bus) -> Result<(), Self::Error> {
        println!("TmpReceiver i16: sync");

        Ok(())
    }
}

#[async_trait]
impl AsyncHandler<i8> for TmpReceiver1 {
    type Error = Error;
    type Response = f32;

    async fn handle(&self, msg: i8, _bus: &Bus) -> Result<Self::Response, Self::Error> {
        Ok(msg as f32)
    }

    async fn sync(&self, _bus: &Bus) -> Result<(), Self::Error> {
        println!("TmpReceiver1 i8: sync");

        Ok(())
    }
}

#[async_trait]
impl AsyncHandler<u8> for TmpReceiver1 {
    type Error = Error;
    type Response = f32;

    async fn handle(&self, msg: u8, _bus: &Bus) -> Result<Self::Response, Self::Error> {
        Ok(msg as f32)
    }
    async fn sync(&self, _bus: &Bus) -> Result<(), Self::Error> {
        println!("TmpReceiver1 u8: sync");

        Ok(())
    }
}

#[async_trait]
impl AsyncHandler<f64> for TmpReceiver2 {
    type Error = Error;
    type Response = f64;

    async fn handle(&self, msg: f64, bus: &Bus) -> Result<Self::Response, Self::Error> {
        let resp1 = bus.request::<_, f32>(100i32, Default::default()).await? as f64;
        let resp2 = bus.request::<_, f32>(200u32, Default::default()).await? as f64;
        let resp3 = bus.request::<_, f32>(300f32, Default::default()).await? as f64;

        Ok(msg + resp1 + resp2 + resp3)
    }

    async fn sync(&self, _bus: &Bus) -> Result<(), Self::Error> {
        println!("TmpReceiver1 f64: sync");

        Ok(())
    }
}

#[async_trait]
impl AsyncHandler<f32> for TmpReceiver2 {
    type Error = Error;
    type Response = f32;

    async fn handle(&self, msg: f32, _bus: &Bus) -> Result<Self::Response, Self::Error> {
        Ok(msg)
    }
    async fn sync(&self, _bus: &Bus) -> Result<(), Self::Error> {
        println!("TmpReceiver2: f32: sync");

        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let (b, poller) = Bus::build()
        .register(TmpReceiver1)
        .subscribe::<i32, receivers::BufferUnorderedAsync<_, f32>, _, _>(8, Default::default())
        .subscribe::<u32, receivers::BufferUnorderedAsync<_, f32>, _, _>(8, Default::default())
        .subscribe::<i16, receivers::BufferUnorderedAsync<_, f32>, _, _>(8, Default::default())
        .subscribe::<u16, receivers::BufferUnorderedAsync<_, f32>, _, _>(8, Default::default())
        .subscribe::<i8, receivers::BufferUnorderedAsync<_, f32>, _, _>(8, Default::default())
        .subscribe::<u8, receivers::BufferUnorderedAsync<_, f32>, _, _>(8, Default::default())
        .done()
        .register(TmpReceiver2)
        .subscribe::<f32, receivers::BufferUnorderedAsync<_, f32>, _, _>(8, Default::default())
        .subscribe::<f64, receivers::BufferUnorderedAsync<_, f64>, _, _>(8, Default::default())
        .done()
        .build();

    println!(
        "{:?}",
        b.request::<_, f64>(1000f64, Default::default()).await
    );

    println!("flush");
    b.flush().await;

    println!("close");
    b.close().await;

    println!("closed");

    poller.await;
    println!("[done]");
}