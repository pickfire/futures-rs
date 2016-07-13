use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, ATOMIC_USIZE_INIT, Ordering};

use {Future, Wake, Tokens};
use slot::{Slot, Token};
use stream::{Stream, StreamResult};
use util;

/// Creates an in-memory channel implementation of the `Stream` trait.
///
/// This method creates a concrete implementation of the `Stream` trait which
/// can be used to send values across threads in a streaming fashion. This
/// channel is unique in that it implements back pressure to ensure that the
/// sender never outpaces the receiver. The `Sender::send` method will only
/// allow sending one message and the next message can only be sent once the
/// first was consumed.
///
/// The `Receiver` returned implements the `Stream` trait and has access to any
/// number of the associated combinators for transforming the result.
pub fn channel<T, E>() -> (Sender<T, E>, Receiver<T, E>)
    where T: Send + 'static,
          E: Send + 'static,
{
    static COUNT: AtomicUsize = ATOMIC_USIZE_INIT;

    let inner = Arc::new(Inner {
        slot: Slot::new(None),
        receiver_gone: AtomicBool::new(false),
        token: COUNT.fetch_add(1, Ordering::SeqCst) + 1,
    });
    let sender = Sender {
        inner: inner.clone(),
    };
    let receiver = Receiver {
        inner: inner,
        on_full_token: None,
    };
    (sender, receiver)
}

/// The transmission end of a channel which is used to send values.
///
/// This is created by the `channel` method in the `stream` module.
pub struct Sender<T, E>
    where T: Send + 'static,
          E: Send + 'static,
{
    inner: Arc<Inner<T, E>>,
}

/// A future returned by the `Sender::send` method which will resolve to the
/// sender once it's available to send another message.
pub struct FutureSender<T, E>
    where T: Send + 'static,
          E: Send + 'static,
{
    sender: Option<Sender<T, E>>,
    data: Option<Result<T, E>>,
}

/// The receiving end of a channel which implements the `Stream` trait.
///
/// This is a concrete implementation of a stream which can be used to represent
/// a stream of values being computed elsewhere. This is created by the
/// `channel` method in the `stream` module.
pub struct Receiver<T, E>
    where T: Send + 'static,
          E: Send + 'static,
{
    inner: Arc<Inner<T, E>>,
    on_full_token: Option<Token>,
}

struct Inner<T, E> {
    slot: Slot<Message<Result<T, E>>>,
    receiver_gone: AtomicBool,
    token: usize,
}

enum Message<T> {
    Data(T),
    Done,
}

pub struct SendError<T, E>(Result<T, E>);

impl<T, E> Stream for Receiver<T, E>
    where T: Send + 'static,
          E: Send + 'static,
{
    type Item = T;
    type Error = E;

    fn poll(&mut self, _tokens: &Tokens) -> Option<StreamResult<T, E>> {
        // if !tokens.may_contain(&Tokens::from_usize(self.inner.token)) {
        //     return None
        // }

        // TODO: disconnect?
        match self.inner.slot.try_consume() {
            Ok(Message::Data(Ok(e))) => Some(Ok(Some(e))),
            Ok(Message::Data(Err(e))) => Some(Err(e)),
            Ok(Message::Done) => Some(Ok(None)),
            Err(..) => None,
        }
    }

    fn schedule(&mut self, wake: Arc<Wake>) {
        if let Some(token) = self.on_full_token.take() {
            self.inner.slot.cancel(token);
        }

        let token = self.inner.token;
        self.on_full_token = Some(self.inner.slot.on_full(move |_| {
            wake.wake(&Tokens::from_usize(token))
        }));
    }
}

impl<T, E> Drop for Receiver<T, E>
    where T: Send + 'static,
          E: Send + 'static,
{
    fn drop(&mut self) {
        self.inner.receiver_gone.store(true, Ordering::SeqCst);
        if let Some(token) = self.on_full_token.take() {
            self.inner.slot.cancel(token);
        }
        self.inner.slot.on_full(|slot| {
            drop(slot.try_consume());
        });
    }
}

impl<T, E> Sender<T, E>
    where T: Send + 'static,
          E: Send + 'static,
{
    /// Sends a new value along this channel to the receiver.
    ///
    /// This method consumes the sender and returns a future which will resolve
    /// to the sender again when the value sent has been consumed.
    pub fn send(self, t: Result<T, E>) -> FutureSender<T, E> {
        FutureSender {
            sender: Some(self),
            data: Some(t),
        }
    }
}

impl<T, E> Drop for Sender<T, E>
    where T: Send + 'static,
          E: Send + 'static,
{
    fn drop(&mut self) {
        self.inner.slot.on_empty(|slot| {
            slot.try_produce(Message::Done).ok().unwrap();
        });
    }
}

impl<T, E> Future for FutureSender<T, E>
    where T: Send + 'static,
          E: Send + 'static,
{
    type Item = Sender<T, E>;
    type Error = SendError<T, E>;

    fn poll(&mut self, _tokens: &Tokens)
            -> Option<Result<Self::Item, Self::Error>> {
        let data = self.data.take().expect("cannot poll FutureSender twice");
        let sender = self.sender.take().expect("cannot poll FutureSender twice");
        match sender.inner.slot.try_produce(Message::Data(data)) {
            Ok(()) => return Some(Ok(sender)),
            Err(e) => {
                self.data = Some(match e.into_inner() {
                    Message::Data(data) => data,
                    Message::Done => panic!(),
                });
                self.sender = Some(sender);
                None
            }
        }
    }

    fn schedule(&mut self, wake: Arc<Wake>) {
        match self.sender {
            Some(ref s) => {
                // TODO: don't drop token?
                s.inner.slot.on_empty(move |_slot| {
                    wake.wake(&Tokens::all());
                });
            }
            None => util::done(wake),
        }
    }
}
