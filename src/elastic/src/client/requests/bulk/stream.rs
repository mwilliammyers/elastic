use std::{
    error::Error as StdError,
    fmt,
    io,
    marker::PhantomData,
    mem,
    time::{
        Duration,
        Instant,
    },
};

use bytes::{
    BufMut,
    BytesMut,
};
use channel::{
    self,
    TryRecvError,
    TrySendError,
};
use fluent_builder::FluentBuilder;
use futures::{
    task::Poll,
    Future,
    Sink,
    Stream,
};
use serde::{
    de::DeserializeOwned,
    ser::Serialize,
};
use tokio::timer::{
    self,
    Delay,
};

use super::{
    BulkOperation,
    BulkRequestBuilder,
    BulkRequestInner,
    Pending,
    WrappedBody,
};
use crate::{
    client::{
        requests::RequestBuilder,
        Client,
        RequestParams,
    },
    error::{
        self,
        Error,
    },
    http::{
        receiver::IsOk,
        sender::AsyncSender,
    },
    params::{
        Index,
        Type,
    },
};

/**
The sending half of a stream of bulk operations.

The sender accepts individual operations and keeps them in a buffer until a timer has expired or the buffer fills up.
*/
pub struct BulkSender<TDocument, TResponse> {
    tx: BulkSenderInner<TResponse>,
    req_template: SenderRequestTemplate<TResponse>,
    in_flight: BulkSenderInFlight<TResponse>,
    timeout: Timeout,
    body: SenderBody,
    _marker: PhantomData<TDocument>,
}

impl<TDocument, TResponse> BulkSender<TDocument, TResponse> {
    pub(super) fn new(
        req_template: SenderRequestTemplate<TResponse>,
        timeout: Timeout,
        body: SenderBody,
    ) -> (Self, BulkReceiver<TResponse>) {
        let (tx, rx) = channel::bounded(1);

        let sender = BulkSender {
            tx: BulkSenderInner(Some(tx)),
            req_template,
            timeout,
            body,
            in_flight: BulkSenderInFlight::ReadyToSend,
            _marker: PhantomData,
        };

        (
            sender,
            BulkReceiver {
                rx: BulkReceiverInner(rx),
            },
        )
    }
}

pub(super) struct SenderRequestTemplate<TResponse> {
    client: Client<AsyncSender>,
    params: RequestParams,
    index: Option<Index<'static>>,
    ty: Option<Type<'static>>,
    _marker: PhantomData<TResponse>,
}

impl<TResponse> SenderRequestTemplate<TResponse> {
    pub(super) fn new(
        client: Client<AsyncSender>,
        params: RequestParams,
        index: Option<Index<'static>>,
        ty: Option<Type<'static>>,
    ) -> Self {
        SenderRequestTemplate {
            client,
            params,
            index,
            ty,
            _marker: PhantomData,
        }
    }

    fn to_request(&self, body: Vec<u8>) -> BulkRequestBuilder<AsyncSender, Vec<u8>, TResponse> {
        RequestBuilder::new(
            self.client.clone(),
            FluentBuilder::new().value(self.params.clone()),
            BulkRequestInner::<Vec<u8>, TResponse> {
                index: self.index.clone(),
                ty: self.ty.clone(),
                body: WrappedBody::new(body),
                _marker: PhantomData,
            },
        )
    }
}

pub(super) struct Timeout {
    duration: Duration,
    delay: Delay,
}

impl Timeout {
    pub(super) fn new(duration: Duration) -> Self {
        let delay = timer::delay(Instant::now() + duration);

        Timeout { duration, delay }
    }

    fn restart(&mut self) {
        self.delay.reset(Instant::now() + self.duration);
    }
}

impl Future for Timeout {
    type Output = Result<(), Error>;

    fn poll(&mut self) -> Poll<Self::Output> {
        self.delay.poll().map_err(error::request)
    }
}

/**
The current state of the `BulkSender`.

The `BulkSender` and `BulkBody` combination means operations can be pushed while a single request is in-flight.
*/
enum BulkSenderInFlight<TResponse> {
    ReadyToSend,
    Pending(Pending<TResponse>),
    Transmitting(Option<TResponse>),
    Transmitted,
}

struct BulkSenderInner<T>(Option<channel::Sender<T>>);
struct BulkReceiverInner<T>(channel::Receiver<T>);

/**
The receiving half of a stream of bulk operations.

The receiver emits complete bulk responses.
*/
pub struct BulkReceiver<TResponse> {
    rx: BulkReceiverInner<TResponse>,
}

pub(super) struct SenderBody {
    scratch: Vec<u8>,
    body: BytesMut,
    size: usize,
}

impl SenderBody {
    pub(super) fn new(size: usize) -> Self {
        SenderBody {
            scratch: Vec::new(),
            size,
            body: BytesMut::with_capacity(size),
        }
    }

    fn take(&mut self) -> BytesMut {
        // Make sure any oversize remaining scratch can be copied to the new buffer
        let size = usize::max(self.scratch.len(), self.size);
        let mut new_body = BytesMut::with_capacity(size);

        // Copy out any scratch into the new buffer
        // This would probably be a single operation that didn't fit
        if !self.scratch.is_empty() {
            new_body.put_slice(&self.scratch);
            self.scratch.clear();
        }

        mem::replace(&mut self.body, new_body)
    }

    fn has_capacity(&self) -> bool {
        self.scratch.is_empty() && self.body.remaining_mut() > 0
    }

    fn is_empty(&self) -> bool {
        self.body.len() == 0
    }

    fn is_full(&self) -> bool {
        !self.scratch.is_empty() || self.body.remaining_mut() == 0
    }

    fn push<TDocument>(&mut self, op: BulkOperation<TDocument>) -> Result<(), io::Error>
    where
        TDocument: Serialize,
    {
        op.write(&mut self.scratch)?;

        // Copy the scratch buffer into the request buffer if it fits
        if self.scratch.len() <= self.body.remaining_mut() {
            self.body.put_slice(&self.scratch);
            self.scratch.clear();

            Ok(())
        }
        // If the body is empty and the buffer doesn't fit, replace the current body buffer
        else if self.body.is_empty() {
            let scratch = mem::replace(&mut self.scratch, Vec::new());
            self.body = BytesMut::from(scratch);

            Ok(())
        }
        // If the buffer doesn't fit, then retain it for the next request
        else {
            Ok(())
        }
    }
}

impl<TDocument, TResponse> Sink<BulkOperation<TDocument>> for BulkSender<TDocument, TResponse>
where
    TDocument: Serialize + Send + 'static,
    TResponse: DeserializeOwned + IsOk + Send + 'static,
{
    type Error = Error;

    fn start_send(&mut self, item: BulkOperation<TDocument>) -> Poll<Result<(), Self::Error>> {
        match self.timeout.poll() {
            // Only respect the timeout if the body is not empty
            Ok(Poll::Ready(())) if !self.body.is_empty() => {
                return match self.poll_flush() {
                    Ok(_) => Poll::Pending(Ok(item)),
                    Err(e) => Poll::Pending(Err(e)),
                };
            }
            // Continue
            Ok(Poll::Ready(_)) | Ok(Poll::Pending) => (),
            Err(e) => return Err(error::request(e)),
        }

        if self.body.has_capacity() {
            self.body.push(item).map_err(error::request)?;
            Poll::Ready(Ok(()))
        } else {
            match self.poll_flush() {
                Ok(_) => Poll::Pending(Ok(item)),
                Err(e) => Poll::Pending(Err(e)),
            }
        }
    }

    fn poll_flush(&mut self) -> Poll<Result<(), Self::Error>> {
        let in_flight = match self.in_flight {
            // The `Sender` is ready to send another request
            BulkSenderInFlight::ReadyToSend => {
                match self.timeout.poll() {
                    // If the timeout hasn't expired and the body isn't full then we're not ready
                    Ok(Poll::Pending) if !self.body.is_full() && !self.body.is_empty() => {
                        return Ok(Poll::Pending);
                    }
                    // Continue
                    Ok(Poll::Pending) => (),
                    // Restart the expired timer
                    Ok(Poll::Ready(())) => self.timeout.restart(),
                    Err(e) => return Err(error::request(e)),
                }

                if self.body.is_empty() {
                    return Ok(Poll::Ready(()));
                }

                debug!("Elasticsearch Bulk Stream: sending a bulk request");

                let body = self.body.take();

                let req = self.req_template.to_request(body.to_vec());
                let pending = req.send();

                BulkSenderInFlight::Pending(pending)
            }
            // A request is pending
            BulkSenderInFlight::Pending(ref mut pending) => {
                let response = ready!(pending.poll());
                BulkSenderInFlight::Transmitting(Some(response))
            }
            // A response is transmitting
            BulkSenderInFlight::Transmitting(ref mut response) => {
                if let Some(item) = response.take() {
                    match self.tx.start_send(item) {
                        Ok(Poll::Ready) => BulkSenderInFlight::Transmitted,
                        Ok(Poll::Pending(item)) => {
                            debug!("Elasticsearch Bulk Stream: waiting for receiver to accept bulk response");

                            *response = Some(item);
                            return Ok(Poll::Pending);
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    BulkSenderInFlight::Transmitted
                }
            }
            // The request has completed
            BulkSenderInFlight::Transmitted => {
                ready!(self.tx.poll_flush());
                BulkSenderInFlight::ReadyToSend
            }
        };

        self.in_flight = in_flight;
        self.poll_flush()
    }

    fn poll_close(&mut self) -> Poll<Result<(), Self::Error>> {
        ready!(self.poll_flush());
        self.tx.close()
    }
}

impl<T> Sink<T> for BulkSenderInner<T>
where
    T: Send,
{
    type Error = Error;

    fn start_send(&mut self, item: T) -> Result<(), Self::Error> {
        self.0
            .as_ref()
            .map(|tx| match tx.try_send(item) {
                Ok(()) => Ok(Poll::Ready),
                Err(TrySendError::Full(item)) => Ok(Poll::Pending(item)),
                Err(TrySendError::Disconnected(_)) => Err(error::request(Disconnected)),
            })
            .unwrap_or_else(|| Err(error::request(Disconnected)))
    }

    fn poll_flush(&mut self) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok())
    }

    fn poll_close(&mut self) -> Poll<Result<(), Self::Error>> {
        self.0 = None;
        Poll::Ready(Ok())
    }
}

impl<TResponse> Stream for BulkReceiver<TResponse>
where
    TResponse: Send,
{
    type Item = Result<TResponse, Error>;

    fn poll_next(&mut self) -> Poll<Option<Self::Item>> {
        self.rx.poll()
    }
}

impl<T> Stream for BulkReceiverInner<T>
where
    T: Send,
{
    type Item = Result<T, Error>;

    fn poll_next(&mut self) -> Poll<Option<Self::Item>> {
        match self.0.try_recv() {
            Ok(item) => Poll::Ready(Some(Ok(item))),
            Err(TryRecvError::Empty) => Poll::Pending,
            // If the channel is disconnected, then we're finished processing
            Err(TryRecvError::Disconnected) => Poll::Ready(None),
        }
    }
}

/**
Alternative disconnected error because `TrySendError` and `TryReceiveError` don't implement `Error`.
*/
#[derive(Debug)]
struct Disconnected;

impl fmt::Display for Disconnected {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("disconnected")
    }
}

impl StdError for Disconnected {
    fn description(&self) -> &str {
        "disconnected"
    }
}
