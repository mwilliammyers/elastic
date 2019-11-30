use std::{
    error::Error as StdError,
    sync::Arc,
};

use futures::{
    future::{
        lazy,
        Either,
    },
    task::Poll,
    Future,
    TryFutureExt,
};
use reqwest::r#async::{
    Client as AsyncHttpClient,
    RequestBuilder as AsyncHttpRequestBuilder,
};
use tokio_threadpool::{
    SpawnHandle,
    ThreadPool,
};

use crate::{
    endpoints::Endpoint,
    error::{
        self,
        Error,
    },
    http::{
        receiver::{
            async_response,
            AsyncResponseBuilder,
        },
        sender::{
            build_reqwest_method,
            build_url,
            NextParams,
            NodeAddresses,
            NodeAddressesInner,
            RequestParams,
            SendableRequest,
            SendableRequestParams,
            Sender,
        },
        AsyncBody,
        AsyncHttpRequest,
        Url,
    },
    private,
};

pub(crate) type AsyncPreSend = dyn Fn(
        &mut AsyncHttpRequest,
    ) -> Box<dyn Future<Output = Result<(), Box<dyn StdError + Send + Sync>>> + Send>
    + Send
    + Sync;

/** An asynchronous request sender. */
#[derive(Clone)]
pub struct AsyncSender {
    pub(crate) http: AsyncHttpClient,
    pub(crate) serde_pool: Option<Arc<ThreadPool>>,
    pub(crate) pre_send: Option<Arc<AsyncPreSend>>,
}

impl private::Sealed for AsyncSender {}

impl AsyncSender {
    pub(crate) fn maybe_async<TFn, TResult>(
        &self,
        f: TFn,
    ) -> Either<SpawnHandle<TResult, Error>, dyn Future<Output = Result<TResult, Error>>>
    where
        TFn: FnOnce() -> Result<TResult, Error> + Send + 'static,
        TResult: Send + 'static,
    {
        if let Some(ref ser_pool) = self.serde_pool {
            Either::A(ser_pool.spawn_handle(lazy(f)))
        } else {
            Either::B(f().next())
        }
    }
}

impl Sender for AsyncSender {
    type Body = AsyncBody;
    type Response = PendingResponse;
    type Params = PendingParams;

    fn send<TEndpoint, TParams, TBody>(
        &self,
        request: SendableRequest<TEndpoint, TParams, TBody>,
    ) -> Self::Response
    where
        TEndpoint: Into<Endpoint<'static, TBody>>,
        TBody: Into<Self::Body> + Send + 'static,
        TParams: Into<Self::Params> + Send + 'static,
    {
        let correlation_id = request.correlation_id;
        let serde_pool = self.serde_pool.clone();
        let params = request.params;
        let Endpoint {
            url, method, body, ..
        } = request.inner.into();

        info!(
            "Elasticsearch Request: correlation_id: '{}', path: '{}'",
            correlation_id,
            url.as_ref()
        );

        let params_future = match params {
            SendableRequestParams::Value(params) => Either::A(Ok(params).next()),
            SendableRequestParams::Builder { params, builder } => {
                let params = params.into().log_err(move |e| {
                    error!(
                        "Elasticsearch Node Selection: correlation_id: '{}', error: '{:?}'",
                        correlation_id, e
                    )
                });

                Either::B(params.and_then(|params| Ok(builder.into_value(move || params))))
            }
        };

        let build_req_future = params_future
            .and_then(move |params| {
                Url::parse(&build_url(&url, &params))
                    .map_err(error::request)
                    .map(|url| (params, url))
            })
            .and_then(move |(params, url)| {
                Ok(AsyncHttpRequest {
                    url,
                    method,
                    headers: params.get_headers(),
                    body: body.map(|body| body.into()),
                })
            });

        let pre_send = self.pre_send.clone();
        let pre_send_future = build_req_future.and_then(move |mut req| {
            if let Some(pre_send) = pre_send {
                Either::A(
                    pre_send(&mut req)
                        .map_err(error::wrapped)
                        .map_err(error::request)
                        .and_then(move |_| Ok(req).next()),
                )
            } else {
                Either::B(Ok(req).next())
            }
        });

        let pre_send_http = self.http.clone();
        let pre_send_future = pre_send_future
            .and_then(move |req| {
                build_reqwest(&pre_send_http, req)
                    .build()
                    .map_err(error::request)
            })
            .log_err(move |e| {
                error!(
                    "Elasticsearch Request: correlation_id: '{}', error: '{:?}'",
                    correlation_id, e
                )
            });

        let req_http = self.http.clone();
        let req_future = pre_send_future.and_then(move |req| {
            req_http
                .execute(req)
                .map_err(error::request)
                .and_then(move |res| {
                    info!(
                        "Elasticsearch Response: correlation_id: '{}', status: '{}'",
                        correlation_id,
                        res.status()
                    );
                    async_response(res, serde_pool).next()
                })
                .log_err(move |e| {
                    error!(
                        "Elasticsearch Response: correlation_id: '{}', error: '{:?}'",
                        correlation_id, e
                    )
                })
        });

        PendingResponse::new(req_future)
    }
}

impl NextParams for NodeAddresses<AsyncSender> {
    type Params = PendingParams;

    fn next(&self) -> Self::Params {
        match self.inner {
            NodeAddressesInner::Static(ref nodes) => PendingParams::new(nodes.next().next()),
            NodeAddressesInner::Sniffed(ref sniffer) => PendingParams::new(sniffer.next()),
        }
    }
}

/** A future returned by calling `next` on an async set of `NodeAddresses`. */
pub struct PendingParams {
    inner: Box<dyn Future<Output = Result<RequestParams, Error>> + Send>,
}

impl PendingParams {
    fn new<F>(fut: F) -> Self
    where
        F: Future<Output = Result<RequestParams, Error>> + Send + 'static,
    {
        PendingParams {
            inner: Box::new(fut),
        }
    }
}

impl Future for PendingParams {
    type Output = Result<RequestParams, Error>;

    fn poll(&mut self) -> Poll<Self::Output> {
        self.inner.poll()
    }
}

impl From<RequestParams> for PendingParams {
    fn from(params: RequestParams) -> Self {
        PendingParams::new(Ok(params).next())
    }
}

/** Build an asynchronous `reqwest::RequestBuilder` from an Elasticsearch request. */
fn build_reqwest(client: &AsyncHttpClient, req: AsyncHttpRequest) -> AsyncHttpRequestBuilder {
    let AsyncHttpRequest {
        url,
        method,
        headers,
        body,
        ..
    } = req;

    let method = build_reqwest_method(method);

    let mut req = client.request(method, url);
    {
        req = req.headers((&*headers).clone());

        if let Some(body) = body {
            req = req.body(body.into_inner());
        }
    }

    req
}

/** A future returned by calling `send` on an `AsyncSender`. */
pub struct PendingResponse {
    inner: Box<dyn Future<Output = Result<AsyncResponseBuilder, Error>> + Send>,
}

impl PendingResponse {
    fn new<F>(fut: F) -> Self
    where
        F: Future<Output = Result<AsyncResponseBuilder, Error>> + Send + 'static,
    {
        PendingResponse {
            inner: Box::new(fut),
        }
    }
}

impl Future for PendingResponse {
    type Output = Result<AsyncResponseBuilder, Error>;

    fn poll(&mut self) -> Poll<Self::Output> {
        self.inner.poll()
    }
}

struct PendingLogErr<F, L> {
    future: F,
    log: Option<L>,
}

impl<F, L> Future for PendingLogErr<F, L>
where
    F: Future,
    L: FnOnce(&F::Error),
{
    type Output = Result<F::Item, F::Error>;

    fn poll(&mut self) -> Poll<Self::Output> {
        match self.future.poll() {
            Err(e) => {
                let log = self.log.take().expect("attempted to poll log twice");
                log(&e);
                Err(e)
            }
            other => other,
        }
    }
}

trait LogErr<E>
where
    Self: Sized,
{
    fn log_err<L>(self, log: L) -> PendingLogErr<Self, L>
    where
        L: FnOnce(&E);
}

impl<F, T, E> LogErr<E> for F
where
    F: Future<Output = Result<T, E>>,
{
    fn log_err<L>(self, log: L) -> PendingLogErr<F, L>
    where
        L: FnOnce(&E),
    {
        PendingLogErr {
            future: self,
            log: Some(log),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::*;

    #[test]
    fn is_send() {
        assert_send::<super::PendingParams>();
        assert_send::<super::PendingResponse>();
    }
}
