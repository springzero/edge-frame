use core::cell::RefCell;
use core::fmt::Debug;

extern crate alloc;
use alloc::rc::Rc;

use log::{info, Level};

use serde::{de::DeserializeOwned, Serialize};

use wasm_bindgen_futures::spawn_local;

use yew::prelude::*;

use crate::redust::*;

#[cfg(feature = "middleware-local")]
pub use local::channel;
#[cfg(feature = "middleware-local")]
use local::*;

#[cfg(feature = "middleware-ws")]
pub use ws::channel;
#[cfg(feature = "middleware-ws")]
use ws::*;

pub type RequestId = u32;

pub fn apply_middleware<S, E, R>(
    store: UseStoreHandle<S>,
    channel: (Rc<RefCell<WebSender<R>>>, Rc<RefCell<WebReceiver<E>>>),
) -> anyhow::Result<UseStoreHandle<S>>
where
    S: Reducible + Clone + Debug + 'static,
    S::Action: for<'a> From<(&'a UseStoreHandle<S>, &'a E)> + Debug,
    R: for<'a> From<(&'a S::Action, RequestId)> + Serialize + Debug + 'static,
    E: DeserializeOwned + Debug + 'static,
{
    let (sender, receiver) = channel;

    let request_id_gen = use_mut_ref(|| 0_u32);

    let store = store.apply(log(Level::Info));

    receive(receiver, store.clone());

    let store = store.apply(send(sender.clone(), request_id_gen));

    Ok(store)
}

fn send<S, R>(
    sender: Rc<RefCell<WebSender<R>>>,
    request_id_gen: Rc<RefCell<RequestId>>,
) -> impl Fn(StoreProvider<S>, S::Action, Rc<dyn Fn(StoreProvider<S>, S::Action)>)
where
    S: Reducible + Clone + Debug,
    R: for<'a> From<(&'a S::Action, RequestId)> + Serialize + Debug + 'static,
{
    move |store, action, dispatcher| {
        let mut request_id_gen = request_id_gen.borrow_mut();
        let request_id = *request_id_gen;
        *request_id_gen += 1;

        if let Some(request) = Some((&action, request_id).into()) {
            info!("Sending request: {:?}", request);

            let sender = sender.clone();

            spawn_local(async move {
                sender.borrow_mut().send(request).await.unwrap();
            });
        }

        dispatcher(store.clone(), action);
    }
}

fn receive<S, E>(receiver: Rc<RefCell<WebReceiver<E>>>, store: UseStoreHandle<S>)
where
    S: Reducible + Clone + Debug + 'static,
    S::Action: for<'a> From<(&'a UseStoreHandle<S>, &'a E)> + Debug,
    E: DeserializeOwned + Debug + 'static,
{
    let store_ref = use_mut_ref(|| None);

    *store_ref.borrow_mut() = Some(store);

    use_effect_with_deps(
        move |_| {
            spawn_local(async move {
                receive_async(&mut receiver.borrow_mut(), store_ref)
                    .await
                    .unwrap();
            });

            || ()
        },
        1, // Will only ever be called once
    );
}

async fn receive_async<S, E>(
    receiver: &mut WebReceiver<E>,
    store_ref: Rc<RefCell<Option<UseStoreHandle<S>>>>,
) -> anyhow::Result<()>
where
    S: Reducible + Clone + Debug + 'static,
    S::Action: for<'a> From<(&'a UseStoreHandle<S>, &'a E)> + Debug,
    E: DeserializeOwned + Debug,
{
    loop {
        let event = receiver.recv().await?;

        info!("Received event: {:?}", event);

        let store_borrow = store_ref.borrow();
        let store: &UseStoreHandle<S> = &store_borrow.as_ref().unwrap();
        if let Some(action) = Some((store, &event).into()) {
            store.dispatch(action);
        }
    }
}

#[cfg(feature = "middleware-local")]
mod local {
    use core::cell::RefCell;

    extern crate alloc;
    use alloc::rc::Rc;

    use yew::use_ref;

    use embassy_sync::channel;

    pub fn channel<R, E>(
        sender: channel::DynamicSender<'static, R>,
        receiver: channel::DynamicReceiver<'static, E>,
    ) -> (Rc<RefCell<WebSender<R>>>, Rc<RefCell<WebReceiver<E>>>)
    where
        R: 'static,
        E: 'static,
    {
        let ws = use_ref(move || {
            (
                Rc::new(RefCell::new(WebSender(sender))),
                Rc::new(RefCell::new(WebReceiver(receiver))),
            )
        });

        (ws.0.clone(), ws.1.clone())
    }

    pub struct WebSender<R>(channel::DynamicSender<'static, R>)
    where
        R: 'static;

    impl<R> WebSender<R>
    where
        R: 'static,
    {
        pub async fn send(&mut self, request: R) -> anyhow::Result<()> {
            self.0.send(request).await;

            Ok(())
        }
    }

    pub struct WebReceiver<E>(channel::DynamicReceiver<'static, E>)
    where
        E: 'static;

    impl<E> WebReceiver<E>
    where
        E: 'static,
    {
        pub async fn recv(&mut self) -> anyhow::Result<E> {
            let event = self.0.recv().await;

            Ok(event)
        }
    }
}

#[cfg(feature = "middleware-ws")]
mod ws {
    use core::cell::RefCell;
    use core::marker::PhantomData;

    extern crate alloc;
    use alloc::rc::Rc;

    use serde::{de::DeserializeOwned, Serialize};

    use futures::stream::{SplitSink, SplitStream};
    use futures::{SinkExt, StreamExt};

    use gloo_net::websocket::{futures::WebSocket, Message};

    use postcard::*;

    use yew::use_ref;

    pub fn channel<R, E>(
        ws_endpoint: &'static str,
    ) -> (Rc<RefCell<WebSender<R>>>, Rc<RefCell<WebReceiver<R>>>)
    where
        R: 'static,
    {
        let ws = use_ref(move || {
            let (sender, receiver) = open(&format!(
                "ws://{}/{}",
                web_sys::window().unwrap().location().host().unwrap(),
                ws_endpoint,
            ))
            .unwrap();

            (
                Rc::new(RefCell::new(sender)),
                Rc::new(RefCell::new(receiver)),
            )
        });

        (ws.0.clone(), ws.1.clone())
    }

    fn open<R, E>(url: &str) -> anyhow::Result<(WebSender<R>, WebReceiver<E>)> {
        let ws = WebSocket::open(url).map_err(|e| anyhow::anyhow!("{}", e))?;

        let (write, read) = ws.split();

        Ok((
            WebSender(write, PhantomData),
            WebReceiver(read, PhantomData),
        ))
    }

    pub struct WebSender<R>(SplitSink<WebSocket, Message>, PhantomData<fn() -> R>);

    impl<R> WebSender<R>
    where
        R: Serialize,
    {
        pub async fn send(&mut self, request: R) -> anyhow::Result<()> {
            self.0
                .send(Message::Bytes(to_allocvec(&request)?))
                .await
                .map_err(|e| anyhow::anyhow!("{}", e))?;

            Ok(())
        }
    }

    pub struct WebReceiver<E>(SplitStream<WebSocket>, PhantomData<fn() -> E>);

    impl<E> WebReceiver<E>
    where
        E: DeserializeOwned,
    {
        pub async fn recv(&mut self) -> anyhow::Result<E> {
            let message = self
                .0
                .next()
                .await
                .unwrap()
                .map_err(|e| anyhow::anyhow!("{}", e))?;

            let event = match message {
                Message::Bytes(data) => from_bytes(&data)?,
                _ => anyhow::bail!("Invalid message format"),
            };

            Ok(event)
        }
    }
}
