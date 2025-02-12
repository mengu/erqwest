use futures::channel::mpsc::{self, Receiver, Sender, UnboundedReceiver};
use futures::future::{AbortHandle, Abortable, OptionFuture};
use futures::{Future, SinkExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rustler::env::SavedTerm;
use rustler::types::binary::NewBinary;
use rustler::types::map;
use rustler::{Atom, Binary, Encoder, Env, ListIterator, LocalPid, NifResult, Term};
use rustler::{MapIterator, NifMap, NifUnitEnum, OwnedEnv, ResourceArc};
use std::borrow::BorrowMut;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, ThreadId};
use std::time::Duration;
use std::{mem, str};
use bytes::Bytes;

use crate::atoms;
use crate::client::ClientResource;
use crate::utils::maybe_timeout;

const DEFAULT_READ_LENGTH: usize = 8 * 1024 * 1024;

#[derive(NifUnitEnum, Clone, Copy, Debug)]
enum Method {
    Options,
    Get,
    Post,
    Put,
    Delete,
    Head,
    Trace,
    Connect,
    Patch,
}

impl From<Method> for reqwest::Method {
    fn from(method: Method) -> Self {
        use Method::*;
        match method {
            Options => reqwest::Method::OPTIONS,
            Get => reqwest::Method::GET,
            Post => reqwest::Method::POST,
            Put => reqwest::Method::PUT,
            Delete => reqwest::Method::DELETE,
            Head => reqwest::Method::HEAD,
            Trace => reqwest::Method::TRACE,
            Connect => reqwest::Method::CONNECT,
            Patch => reqwest::Method::PATCH,
        }
    }
}

#[derive(NifMap)]
struct ReqBase {
    url: String,
    method: Method,
}

#[derive(NifUnitEnum, Debug)]
enum ErrorCode {
    Cancelled,
    Url,
    Request,
    Redirect,
    Connect,
    Timeout,
    Body,
    Unknown,
}

#[derive(NifMap, Debug)]
struct Error {
    code: ErrorCode,
    reason: String,
}

impl Error {
    fn from_reason(code: ErrorCode, reason: impl ToString) -> Error {
        Error {
            code,
            reason: reason.to_string(),
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Error {
        use ErrorCode::*;
        let code = if e.is_timeout() {
            Timeout
        } else if e.is_redirect() {
            Redirect
        } else if e.is_connect() {
            Connect
        } else if e.is_request() {
            Request
        } else if e.is_body() {
            Body
        } else {
            Unknown
        };
        Error::from_reason(code, e)
    }
}

/// To store an erlang term we need an `OwnedEnv` too.
struct CallerRef {
    env: OwnedEnv,
    ref_: SavedTerm,
}

impl<'a> Into<CallerRef> for Term<'a> {
    fn into(self) -> CallerRef {
        let env = OwnedEnv::new();
        CallerRef {
            ref_: env.save(self),
            env,
        }
    }
}

impl Encoder for CallerRef {
    fn encode<'a>(&self, dest: Env<'a>) -> Term<'a> {
        self.env.run(|env| self.ref_.load(env).in_env(dest))
    }
}

/// Sent when erlang is streaming the request body
enum SendCmd {
    Send(OwnedEnv, SavedTerm),
    FinishSend,
}

/// Options for reading a chunk of the response body
struct ReadOpts {
    length: usize,
    period: Option<Duration>,
}

enum IsFin {
    Fin,
    NoFin,
}

/// Helper for storing/encoding an HTTP response
struct Resp {
    status: u16,
    headers: HeaderMap<HeaderValue>,
    body: Option<Bytes>,
}

impl Resp {
    fn encode(self, env: Env) -> Term {
        let headers1: Vec<_> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let mut k1 = NewBinary::new(env, k.as_str().as_bytes().len());
                k1.as_mut_slice().copy_from_slice(k.as_str().as_bytes());
                let mut v1 = NewBinary::new(env, v.as_bytes().len());
                v1.as_mut_slice().copy_from_slice(v.as_bytes());
                (Term::from(k1), Term::from(v1))
            })
            .collect();
        let mut map = map::map_new(env);
        map = map
            .map_put(atoms::status().encode(env), self.status.encode(env))
            .unwrap();
        map = map
            .map_put(atoms::headers().encode(env), headers1.encode(env))
            .unwrap();
        if let Some(bytes) = self.body {
            let mut body = NewBinary::new(env, bytes.len());
            body.as_mut_slice().copy_from_slice(&bytes);
            map = map.map_put(atoms::body().encode(env), body.into()).unwrap();
        }
        map.encode(env)
    }
}

struct ReqData {
    client: reqwest::Client,
    env: OwnedEnv,
    headers: Vec<(SavedTerm, SavedTerm)>,
    url: SavedTerm,
    method: Method,
    body: Option<ReqBody>,
    timeout: Option<Duration>,
}

impl ReqData {
    fn decode(self) -> Result<reqwest::RequestBuilder, Error> {
        let ReqData {
            client,
            env,
            headers,
            url,
            method,
            body,
            timeout,
        } = self;
        // we use unwrap for the binaries we checked the types of before saving
        env.run(|e| {
            let bin = url.load(e).decode::<Binary>().unwrap();
            let s = str::from_utf8(&bin).map_err(|e| Error::from_reason(ErrorCode::Url, e))?;
            let url = reqwest::Url::parse(s).map_err(|e| Error::from_reason(ErrorCode::Url, e))?;
            let mut builder = client.request(method.into(), url);
            for (k, v) in headers {
                let k = HeaderName::from_bytes(&k.load(e).decode::<Binary>().unwrap())
                    .map_err(|e| Error::from_reason(ErrorCode::Request, e))?;
                let v = HeaderValue::from_bytes(&v.load(e).decode::<Binary>().unwrap())
                    .map_err(|e| Error::from_reason(ErrorCode::Request, e))?;
                builder = builder.header(k, v);
            }
            if let Some(timeout) = timeout {
                builder = builder.timeout(timeout);
            }
            match body {
                Some(ReqBody::Complete(iodata)) => {
                    // we don't know if this is valid iodata()
                    let iodata = iodata
                        .load(e)
                        .decode_as_binary()
                        .map_err(|_| Error::from_reason(ErrorCode::Request, "bad request body"))?;
                    builder = builder.body(iodata.to_vec());
                }
                Some(ReqBody::Stream(rx)) => builder = builder.body(reqwest::Body::wrap_stream(rx)),
                None => (),
            }
            Ok(builder)
        })
    }
}

struct Req {
    caller_ref: Option<CallerRef>,
    caller_pid: LocalPid,
    initial_thread: ThreadId,
    /// An indicator for whether the future was dropped. This doesn't strictly
    /// need to be an atomic since we only access it from `initial_thread`.
    dropped_on_initial_thread: Arc<AtomicBool>,
    /// The channels we use to feed the request body to `reqwest`: The other end
    /// of the `Sender` is converted to a Stream and given to `reqwest`. We get
    /// new data from erlang on the receiver and feed it to the sender. This
    /// allows us to provide backpressure by replying to erlang after each chunk
    /// is successfully `fed`.
    req_body_channels: Option<(
        Sender<Result<Vec<u8>, Infallible>>,
        UnboundedReceiver<SendCmd>,
    )>,
    resp_stream_rx: Option<UnboundedReceiver<ReadOpts>>,
}

impl Req {
    /// Creating an `OwnedEnv` has a (small) cost. When it's time to send the
    /// final message, we exploit the fact that `CallerRef` has an `OwnedEnv`
    /// that will no longer be needed. `take`ing the `CallerRef` signals to the
    /// `Drop` implementation that a final reply has been sent and there is no
    /// need to send a `cancelled` message.
    fn reply_final<F>(&mut self, f: F)
    where
        F: for<'a> FnOnce(Env<'a>, Term<'a>) -> Term<'a>,
    {
        let CallerRef { mut env, ref_ } = self.caller_ref.take().unwrap();
        env.send_and_clear(&self.caller_pid, |env| f(env, ref_.load(env)))
    }
    fn reply_error(&mut self, e: Error) {
        self.reply_final(|env, ref_| {
            (atoms::erqwest_response(), ref_, atoms::error(), e).encode(env)
        })
    }
    fn reply_none(&mut self) {
        self.caller_ref.take().unwrap();
    }
    async fn run(mut self, req_data: ReqData) {
        let builder = match req_data.decode() {
            Ok(builder) => builder,
            Err(e) => {
                self.reply_error(e);
                return;
            }
        };
        let resp = builder.send();
        tokio::pin!(resp);
        let mut res = if let Some((mut tx, rx)) = self.req_body_channels.take() {
            match self.stream_req(&mut resp, &mut tx, rx).await {
                Some(Ok(res)) => res,
                Some(Err(e)) => {
                    // drop the request future before the tx, since
                    // closing the tx means "complete the request".
                    drop(resp);
                    drop(tx);
                    self.reply_error(e.into());
                    return;
                }
                None => {
                    // drop the request future before the tx, since
                    // closing the tx means "complete the request".
                    drop(resp);
                    drop(tx);
                    // the client is not waiting for a reply (eg. has
                    // cancelled), so we don't reply
                    self.reply_none();
                    return;
                }
            }
        } else {
            match resp.await {
                Ok(res) => res,
                Err(e) => {
                    self.reply_error(e.into());
                    return;
                }
            }
        };
        let status = res.status().as_u16();
        // "steal" the headers to avoid a copy
        let mut headers = HeaderMap::new();
        mem::swap(res.headers_mut(), &mut headers);
        if let Some(rx) = self.resp_stream_rx.take() {
            let partial_resp = Resp {
                status,
                headers,
                body: None,
            };
            self.stream_resp(res, rx, partial_resp).await;
        } else {
            match res.bytes().await {
                Ok(bytes) => {
                    let resp = Resp {
                        status,
                        headers,
                        body: Some(bytes),
                    };
                    self.reply_final(|env, ref_| {
                        (
                            atoms::erqwest_response(),
                            ref_,
                            atoms::reply(),
                            resp.encode(env),
                        )
                            .encode(env)
                    });
                }
                Err(e) => self.reply_error(e.into()),
            }
        }
    }
    /// Stream the request body and wait for the response. These two things need
    /// to be combined, since a response can come at any time (even before the
    /// request body is complete). Return values: `Ok(reply | error)` => send a
    /// reply message, `None` => the stream was cancelled, end without replying.
    async fn stream_req(
        &mut self,
        mut resp: &mut Pin<&mut impl Future<Output = reqwest::Result<reqwest::Response>>>,
        tx: &mut Sender<Result<Vec<u8>, Infallible>>,
        mut rx: UnboundedReceiver<SendCmd>,
    ) -> Option<Result<reqwest::Response, Error>> {
        let env = OwnedEnv::new();
        let term_next = env.run(|e| {
            let term = (
                atoms::erqwest_response(),
                &self.caller_ref.as_ref().unwrap(),
                atoms::next(),
            )
                .encode(e);
            e.send(&self.caller_pid, term);
            env.save(term)
        });
        let mut fin = false;
        loop {
            tokio::select! {
                next = rx.next(), if !fin =>
                    match next {
                        Some(SendCmd::Send(term_env, term)) => {
                            let data = term_env.run(|e| term.load(e).decode_as_binary().map(|d| d.to_vec()).map_err(|_|
                               Error::from_reason(
                                    ErrorCode::Request,
                                    "invalid iodata"
                               )
                            ));
                            let feed = match data {
                                Err(e) => return Some(Err(e)),
                                Ok(data) => tx.feed(Ok(data))
                            };
                            tokio::select! {
                                Ok(()) = feed =>
                                    env.run(|env| env.send(&self.caller_pid, term_next.load(env))),
                                // the caller is waiting for a response so we can reply immediately
                                res = &mut resp => return Some(res.map_err(Error::from))
                            }
                        },
                        Some(SendCmd::FinishSend) => {
                            tx.close_channel();
                            // now we just wait for the response
                            fin = true;
                        },
                        None => {
                            // the caller has not asked for a response and will
                            // never be able to, exit without replying
                            return None
                        }
                },
                res = &mut resp => {
                    if fin {
                        // the caller is waiting for a response so reply immediately
                        return Some(res.map_err(Error::from))
                    } else {
                        // the caller is not expecting a response yet so wait for the next command
                        if rx.next().await.is_none() {
                            // the caller has not asked for a response and never
                            // can, so we exit without replying
                            return None
                        } else {
                            return Some(res.map_err(Error::from))
                        }
                    }
                }
            }
        }
    }
    /// Stream the response body. This is always called last, so we are
    /// responsible for sending the final message (reply, error, or nothing if
    /// streaming was cancelled).
    async fn stream_resp(
        &mut self,
        mut resp: reqwest::Response,
        mut rx: UnboundedReceiver<ReadOpts>,
        partial_resp: Resp,
    ) {
        let mut env = OwnedEnv::new();
        env.run(|env| {
            env.send(
                &self.caller_pid,
                (
                    atoms::erqwest_response(),
                    self.caller_ref.as_ref().unwrap(),
                    atoms::reply(),
                    partial_resp.encode(env),
                )
                    .encode(env),
            )
        });
        let mut buf = Vec::new();
        loop {
            match rx.next().await {
                Some(opts) => {
                    buf.clear();
                    // TODO: use stream instead of resp directly
                    match stream_response_chunk(&mut resp, opts, &mut buf).await {
                        Ok(res) => {
                            match res {
                                IsFin::NoFin => {
                                    env.run(|env| {
                                        let mut bin = NewBinary::new(env, buf.len());
                                        bin.as_mut_slice().copy_from_slice(&buf);
                                        env.send(
                                            &self.caller_pid,
                                            (
                                                atoms::erqwest_response(),
                                                &self.caller_ref.as_ref().unwrap(),
                                                atoms::chunk(),
                                                Term::from(bin),
                                            )
                                                .encode(env),
                                        )
                                    });
                                    env.clear();
                                }
                                IsFin::Fin => {
                                    // Before we send the reply, drop the rx to make
                                    // sure that further calls to `read` fail
                                    drop(rx);
                                    self.reply_final(|env, ref_| {
                                        let mut bin = NewBinary::new(env, buf.len());
                                        bin.as_mut_slice().copy_from_slice(&buf);
                                        (
                                            atoms::erqwest_response(),
                                            ref_,
                                            atoms::fin(),
                                            Term::from(bin),
                                        )
                                            .encode(env)
                                    });
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            // Before we send the reply, drop the rx to make
                            // sure that further calls to `read` fail
                            drop(rx);
                            self.reply_error(e.into());
                            return;
                        }
                    }
                }
                None => {
                    // The caller is not awaiting a response and never will
                    self.reply_none();
                    return;
                }
            }
        }
    }
}

impl Drop for Req {
    fn drop(&mut self) {
        if self.caller_ref.is_some() {
            if thread::current().id() == self.initial_thread {
                // We are still on the initial thread, which means the future
                // was not spawned. We can't send a message from this thread
                // (managed by the VM) so we set this flag and
                // `req` returns BadArg.
                self.dropped_on_initial_thread
                    .borrow_mut()
                    .store(true, Ordering::Relaxed);
            } else {
                self.reply_error(Error::from_reason(ErrorCode::Cancelled, "future dropped"));
            }
        }
    }
}

async fn stream_response_chunk(
    response: &mut reqwest::Response,
    opts: ReadOpts,
    buf: &mut Vec<u8>, // passed in so we can reuse the memory allocation between chunks
) -> Result<IsFin, Error> {
    let timeout = OptionFuture::from(opts.period.map(tokio::time::sleep));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            // TODO: is this cancellation safe? maybe safer to use a stream which is guaranteed?
            res = response.chunk() => match res {
                Ok(Some(chunk)) => {
                    buf.extend_from_slice(&chunk);
                    if buf.len() >= opts.length {
                        return Ok(IsFin::NoFin);
                    }
                }
                Ok(None) => return Ok(IsFin::Fin),
                Err(e) => return Err(e.into()),
            },
            Some(()) = &mut timeout => return Ok(IsFin::NoFin)
        }
    }
}

pub struct ReqHandle {
    abort_handle: AbortHandle,
    req_body_tx: Option<mpsc::UnboundedSender<SendCmd>>,
    resp_stream_tx: Option<mpsc::UnboundedSender<ReadOpts>>,
}

/// Helper for decoding the `body` opt
#[derive(NifUnitEnum, PartialEq)]
enum StreamBody {
    Stream,
}

/// Helper for decoding the `response_body` opt
#[derive(NifUnitEnum)]
enum ResponseBody {
    Stream,
    Complete,
}

enum ReqBody {
    Complete(SavedTerm),
    Stream(Receiver<Result<Vec<u8>, Infallible>>),
}

#[rustler::nif]
fn req(
    env: Env,
    resource: ResourceArc<ClientResource>,
    pid: LocalPid,
    caller_ref: Term,
    opts: Term,
) -> NifResult<ResourceArc<ReqHandle>> {
    // returns BadArg if the client was already closed with close_client
    let client = resource
        .client
        .read()
        .unwrap()
        .as_ref()
        .ok_or(rustler::Error::BadArg)?
        .clone();
    let mut req_body_tx = None;
    let mut req_body_channels = None;
    let mut resp_stream_tx = None;
    let mut resp_stream_rx = None;
    let mut headers = None;
    let mut url = None;
    let mut body = None;
    let mut timeout = None;
    let mut method = None;
    let owned_env = OwnedEnv::new();

    for (k, v) in opts.decode::<MapIterator>()? {
        let k: Atom = k.decode()?;
        if k == atoms::url() {
            url = Some(owned_env.save(v.decode::<Binary>()?.to_term(env)));
        } else if k == atoms::method() {
            method = Some(v.decode()?);
        } else if k == atoms::headers() {
            let mut owned_headers = Vec::new();
            for h in v.decode::<ListIterator>()? {
                let (hk, hv): (Binary, Binary) = h.decode()?;
                owned_headers.push((
                    owned_env.save(hk.to_term(env)),
                    owned_env.save(hv.to_term(env)),
                ));
            }
            headers = Some(owned_headers);
        } else if k == atoms::body() {
            if v.decode::<StreamBody>().is_ok() {
                let (tx, rx) = mpsc::channel::<Result<Vec<u8>, Infallible>>(0);
                let (body_tx, body_rx0) = mpsc::unbounded();
                body = Some(ReqBody::Stream(rx));
                req_body_tx = Some(body_tx);
                req_body_channels = Some((tx, body_rx0));
            } else {
                body = Some(ReqBody::Complete(
                    // we don't validate that this is a binary, because it might also be iodata()
                    owned_env.save(v),
                ));
            }
        } else if k == atoms::response_body() {
            match v.decode()? {
                ResponseBody::Complete => (),
                ResponseBody::Stream => {
                    let (tx, rx) = mpsc::unbounded();
                    resp_stream_tx = Some(tx);
                    resp_stream_rx = Some(rx);
                }
            }
        } else if k == atoms::timeout() {
            timeout = maybe_timeout(v)?;
        } else {
            return Err(rustler::Error::RaiseTerm(Box::new((atoms::bad_opt(), k))));
        }
    }

    req_consume_timeslice(env, headers.as_ref().map(|h| h.len()).unwrap_or(0));

    let req_data = ReqData {
        client,
        env: owned_env,
        headers: headers.unwrap_or_default(),
        url: url.ok_or(rustler::Error::BadArg)?,
        method: method.ok_or(rustler::Error::BadArg)?,
        body,
        timeout,
    };
    let req = Req {
        caller_ref: Some(caller_ref.into()),
        caller_pid: pid,
        dropped_on_initial_thread: Arc::new(AtomicBool::new(false)),
        req_body_channels,
        resp_stream_rx,
        initial_thread: thread::current().id(),
    };
    // This allows us to detect if the future was immediately dropped (ie. not
    // sent to another thread), which indicates that the Runtime is shutting
    // down or has shut down.
    let dropped_on_initial_thread = req.dropped_on_initial_thread.clone();
    let fut = req.run(req_data);
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    resource
        .runtime
        .spawn(Abortable::new(fut, abort_registration));
    if dropped_on_initial_thread.load(Ordering::Relaxed) {
        Err(rustler::Error::RaiseAtom("bad_runtime"))
    } else {
        Ok(ResourceArc::new(ReqHandle {
            abort_handle,
            req_body_tx,
            resp_stream_tx,
        }))
    }
}

/// Give the scheduler an estimate of how much time the `req` call has used.
/// Since copying binaries is cheap (due to refcounting), the main variable is
/// the number of headers. Taking numbers from the `time_nifs` testcase on my
/// machine, `req` with no headers takes ~30 µs and with 100 headers ~90 µs, so
/// let's say 0.6 µs per header. A "timeslice" is 1 ms.
fn req_consume_timeslice(env: Env, headers: usize) {
    let percent = (300 + (6 * headers)) / 100;
    if percent > 0 {
        rustler::schedule::consume_timeslice(env, std::cmp::min(percent, 100) as i32);
    }
}

/// Intended to be used by `erqwest_async`, and causes the future to be dropped
/// ASAP.
#[rustler::nif]
fn cancel(req_handle: ResourceArc<ReqHandle>) -> NifResult<Atom> {
    req_handle.abort_handle.abort();
    Ok(atoms::ok())
}

/// Intended to be used by `erqwest` (the sync interface). It is unable to cause
/// an immediate interruption (eg. if the future is waiting for a data from the
/// server), but that's OK because there is no way for the user to express that
/// through the sync API (apart from killing the process).
#[rustler::nif]
fn cancel_stream(req_handle: ResourceArc<ReqHandle>) -> Atom {
    if let Some(body_tx) = req_handle.req_body_tx.as_ref() {
        body_tx.close_channel();
    }
    if let Some(resp_stream_tx) = req_handle.resp_stream_tx.as_ref() {
        resp_stream_tx.close_channel();
    }
    atoms::ok()
}

/// Stream a chunk of the request body
#[rustler::nif]
fn send<'a>(req_handle: ResourceArc<ReqHandle>, data: Term<'a>) -> NifResult<Atom> {
    if let Some(body_tx) = req_handle.req_body_tx.as_ref() {
        let env = OwnedEnv::new();
        let term = env.save(data);
        let cmd = SendCmd::Send(env, term);
        if body_tx.unbounded_send(cmd).is_ok() {
            return Ok(atoms::ok());
        }
    }
    Err(rustler::Error::BadArg)
}

#[rustler::nif]
fn finish_send(req_handle: ResourceArc<ReqHandle>) -> NifResult<Atom> {
    if let Some(body_tx) = req_handle.req_body_tx.as_ref() {
        if body_tx.unbounded_send(SendCmd::FinishSend).is_ok() {
            body_tx.close_channel();
            return Ok(atoms::ok());
        }
    }
    Err(rustler::Error::BadArg)
}

/// Stream a chunk of the response body
#[rustler::nif]
fn read<'a>(
    env: Env<'a>,
    req_handle: ResourceArc<ReqHandle>,
    opts_or_cancel: Term,
) -> NifResult<Term<'a>> {
    let mut period = None;
    let mut length = DEFAULT_READ_LENGTH;
    for (k, v) in opts_or_cancel.decode::<MapIterator>()? {
        let k: Atom = k.decode()?;
        if k == atoms::length() {
            length = v.decode()?;
        } else if k == atoms::period() {
            period = maybe_timeout(v)?;
        } else {
            return Err(rustler::Error::RaiseTerm(Box::new((atoms::bad_opt(), k))));
        }
    }
    let opts = ReadOpts { length, period };
    if let Some(resp_stream_tx) = req_handle.resp_stream_tx.as_ref() {
        if resp_stream_tx.unbounded_send(opts).is_ok() {
            return Ok(atoms::ok().encode(env));
        }
    }
    Err(rustler::Error::BadArg)
}
