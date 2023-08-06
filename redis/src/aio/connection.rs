#[cfg(feature = "async-std-comp")]
use super::async_std;
use super::ConnectionLike;
use super::{authenticate, AsyncStream, RedisRuntime};
use crate::cmd::{cmd, Cmd};
use crate::connection::{ConnectionAddr, ConnectionInfo, RedisConnectionInfo};
#[cfg(any(feature = "tokio-comp", feature = "async-std-comp"))]
use crate::parser::ValueCodec;
use crate::types::{ErrorKind, RedisError, RedisFuture, RedisResult, Value};
use crate::{from_redis_value, FromRedisValue, Msg, ToRedisArgs};
#[cfg(all(not(feature = "tokio-comp"), feature = "async-std-comp"))]
use ::async_std::net::ToSocketAddrs;
use ::tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
#[cfg(feature = "tokio-comp")]
use ::tokio::net::lookup_host;
use combine::{parser::combinator::AnySendSyncPartialState, stream::PointerOffset};
use futures::channel::mpsc::{channel, SendError};
use futures::channel::{mpsc, oneshot};
use futures_util::future::{select_ok, FutureExt};
use futures_util::stream::{SplitSink, SplitStream, Stream, StreamExt};
use futures_util::{Future, Sink, SinkExt};
use pin_project_lite::pin_project;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::ready;
#[cfg(any(feature = "tokio-comp", feature = "async-std-comp"))]
use tokio_util::codec::{Decoder, Framed};

/// Represents a stateful redis TCP connection.
pub struct Connection<C = Pin<Box<dyn AsyncStream + Send + Sync>>> {
    con: C,
    buf: Vec<u8>,
    decoder: combine::stream::Decoder<AnySendSyncPartialState, PointerOffset<[u8]>>,
    db: i64,

    // Flag indicating whether the connection was left in the PubSub state after dropping `PubSub`.
    //
    // This flag is checked when attempting to send a command, and if it's raised, we attempt to
    // exit the pubsub state before executing the new request.
    pubsub: bool,
}

fn assert_sync<T: Sync>() {}

#[allow(unused)]
fn test() {
    assert_sync::<Connection>();
}

impl<C> Connection<C> {
    pub(crate) fn map<D>(self, f: impl FnOnce(C) -> D) -> Connection<D> {
        let Self {
            con,
            buf,
            decoder,
            db,
            pubsub,
        } = self;
        Connection {
            con: f(con),
            buf,
            decoder,
            db,
            pubsub,
        }
    }
}

impl<C> Connection<C>
where
    C: Unpin + AsyncRead + AsyncWrite + Send,
{
    /// Constructs a new `Connection` out of a `AsyncRead + AsyncWrite` object
    /// and a `RedisConnectionInfo`
    pub async fn new(connection_info: &RedisConnectionInfo, con: C) -> RedisResult<Self> {
        let mut rv = Connection {
            con,
            buf: Vec::new(),
            decoder: combine::stream::Decoder::new(),
            db: connection_info.db,
            pubsub: false,
        };
        authenticate(connection_info, &mut rv).await?;
        Ok(rv)
    }

    /// Converts this [`Connection`] into [`PubSub`].
    pub fn into_pubsub(self) -> PubSub<C> {
        PubSub::new(self)
    }

    /// Converts this [`Connection`] into [`Monitor`].
    pub fn into_monitor(self) -> Monitor<C> {
        Monitor::new(self)
    }

    /// Fetches a single response from the connection.
    async fn read_response(&mut self) -> RedisResult<Value> {
        crate::parser::parse_redis_value_async(&mut self.decoder, &mut self.con).await
    }

    /// Brings [`Connection`] out of `PubSub` mode.
    ///
    /// This will unsubscribe this [`Connection`] from all subscriptions.
    ///
    /// If this function returns error then on all command send tries will be performed attempt
    /// to exit from `PubSub` mode until it will be successful.
    async fn exit_pubsub(&mut self) -> RedisResult<()> {
        let res = self.clear_active_subscriptions().await;
        if res.is_ok() {
            self.pubsub = false;
        } else {
            // Raise the pubsub flag to indicate the connection is "stuck" in that state.
            self.pubsub = true;
        }

        res
    }

    /// Get the inner connection out of a PubSub
    ///
    /// Any active subscriptions are unsubscribed. In the event of an error, the connection is
    /// dropped.
    async fn clear_active_subscriptions(&mut self) -> RedisResult<()> {
        // Responses to unsubscribe commands return in a 3-tuple with values
        // ("unsubscribe" or "punsubscribe", name of subscription removed, count of remaining subs).
        // The "count of remaining subs" includes both pattern subscriptions and non pattern
        // subscriptions. Thus, to accurately drain all unsubscribe messages received from the
        // server, both commands need to be executed at once.
        {
            // Prepare both unsubscribe commands
            let unsubscribe = crate::Pipeline::new()
                .add_command(cmd("UNSUBSCRIBE"))
                .add_command(cmd("PUNSUBSCRIBE"))
                .get_packed_pipeline();

            // Execute commands
            self.con.write_all(&unsubscribe).await?;
        }

        // Receive responses
        //
        // There will be at minimum two responses - 1 for each of punsubscribe and unsubscribe
        // commands. There may be more responses if there are active subscriptions. In this case,
        // messages are received until the _subscription count_ in the responses reach zero.
        let mut received_unsub = false;
        let mut received_punsub = false;
        loop {
            let res: (Vec<u8>, (), isize) = from_redis_value(&self.read_response().await?)?;

            match res.0.first() {
                Some(&b'u') => received_unsub = true,
                Some(&b'p') => received_punsub = true,
                _ => (),
            }

            if received_unsub && received_punsub && res.2 == 0 {
                break;
            }
        }

        // Finally, the connection is back in its normal state since all subscriptions were
        // cancelled *and* all unsubscribe messages were received.
        Ok(())
    }
}

#[cfg(feature = "async-std-comp")]
#[cfg_attr(docsrs, doc(cfg(feature = "async-std-comp")))]
impl<C> Connection<async_std::AsyncStdWrapped<C>>
where
    C: Unpin + ::async_std::io::Read + ::async_std::io::Write + Send,
{
    /// Constructs a new `Connection` out of a `async_std::io::AsyncRead + async_std::io::AsyncWrite` object
    /// and a `RedisConnectionInfo`
    pub async fn new_async_std(connection_info: &RedisConnectionInfo, con: C) -> RedisResult<Self> {
        Connection::new(connection_info, async_std::AsyncStdWrapped::new(con)).await
    }
}

pub(crate) async fn connect<C>(connection_info: &ConnectionInfo) -> RedisResult<Connection<C>>
where
    C: Unpin + RedisRuntime + AsyncRead + AsyncWrite + Send,
{
    let con = connect_simple::<C>(connection_info).await?;
    Connection::new(&connection_info.redis, con).await
}

impl<C> ConnectionLike for Connection<C>
where
    C: Unpin + AsyncRead + AsyncWrite + Send,
{
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        (async move {
            if self.pubsub {
                self.exit_pubsub().await?;
            }
            self.buf.clear();
            cmd.write_packed_command(&mut self.buf);
            self.con.write_all(&self.buf).await?;
            self.read_response().await
        })
        .boxed()
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a crate::Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        (async move {
            if self.pubsub {
                self.exit_pubsub().await?;
            }

            self.buf.clear();
            cmd.write_packed_pipeline(&mut self.buf);
            self.con.write_all(&self.buf).await?;

            let mut first_err = None;

            for _ in 0..offset {
                let response = self.read_response().await;
                if let Err(err) = response {
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                }
            }

            let mut rv = Vec::with_capacity(count);
            for _ in 0..count {
                let response = self.read_response().await;
                match response {
                    Ok(item) => {
                        rv.push(item);
                    }
                    Err(err) => {
                        if first_err.is_none() {
                            first_err = Some(err);
                        }
                    }
                }
            }

            if let Some(err) = first_err {
                Err(err)
            } else {
                Ok(rv)
            }
        })
        .boxed()
    }

    fn get_db(&self) -> i64 {
        self.db
    }
}

pin_project! {
    /// Represents a pubsub connection.
    #[project = PubSubProj]
    pub struct PubSub<C = Pin<Box<dyn AsyncStream + Send + Sync>>> {
        #[pin]
        framed: Framed<C, ValueCodec>,
        queue: VecDeque<Msg>,
    }
}

fn next_response_err() -> RedisError {
    RedisError::from((
        ErrorKind::ResponseDropped,
        "Response was dropped with stream",
    ))
}

impl<C> PubSub<C>
where
    C: Unpin + AsyncRead + AsyncWrite + Send,
{
    fn new(con: Connection<C>) -> PubSub<C> {
        let framed = ValueCodec::default().framed(con.con);

        PubSub {
            framed,
            queue: Default::default(),
        }
    }

    /// Splits the [`PubSub`] into a [`SplitPubSubSink`] and [`SplitPubSubStream`].
    pub fn split(self) -> (SplitPubSubSink<C>, SplitPubSubStream<C>) {
        SplitPubSubSink::new(self.framed)
    }

    /// Query the connection with a command and retrieves the response.
    pub async fn query<'a, T>(&'a mut self, cmd: &'a Cmd) -> RedisResult<T>
    where
        T: FromRedisValue,
    {
        let mut out = Vec::new();

        cmd.write_packed_command(&mut out);
        self.framed.send(out).await?;

        self.next_response().await
    }

    async fn next_response<T>(&mut self) -> RedisResult<T>
    where
        T: FromRedisValue,
    {
        while let Some(item) = self.framed.next().await {
            match item {
                Ok(Ok(value)) => match Msg::from_value(&value) {
                    Some(msg) => self.queue.push_front(msg),
                    _ => return from_redis_value(&value),
                },
                _ => break,
            }
        }

        Err(next_response_err())
    }

    /// Subscribes to a new channel.
    pub async fn subscribe<T: ToRedisArgs>(&mut self, channel: T) -> RedisResult<()> {
        self.query(cmd("SUBSCRIBE").arg(channel)).await
    }

    /// Subscribes to a new channel with a pattern.
    pub async fn psubscribe<T: ToRedisArgs>(&mut self, pchannel: T) -> RedisResult<()> {
        self.query(cmd("PSUBSCRIBE").arg(pchannel)).await
    }

    /// Unsubscribes from a channel.
    pub async fn unsubscribe<T: ToRedisArgs>(&mut self, channel: T) -> RedisResult<()> {
        self.query(cmd("UNSUBSCRIBE").arg(channel)).await
    }

    /// Unsubscribes from a channel with a pattern.
    pub async fn punsubscribe<T: ToRedisArgs>(&mut self, pchannel: T) -> RedisResult<()> {
        self.query(cmd("PUNSUBSCRIBE").arg(pchannel)).await
    }
}

impl Stream for PubSub {
    type Item = Result<Msg, RedisError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let PubSubProj { mut framed, queue } = self.project();

        if let Some(msg) = queue.pop_back() {
            return std::task::Poll::Ready(Some(Ok(msg)));
        }

        loop {
            match ready!(framed.as_mut().poll_next(cx)) {
                Some(Ok(Ok(value))) => {
                    if let Some(msg) = Msg::from_value(&value) {
                        return std::task::Poll::Ready(Some(Ok(msg)));
                    }
                }
                Some(Err(err)) | Some(Ok(Err(err))) => {
                    return std::task::Poll::Ready(Some(Err(err)));
                }
                None => return std::task::Poll::Ready(None),
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.framed.size_hint()
    }
}

/// Represents the transmitting side of a pubsub connection.
/// The corresponding [`SplitPubSubStream`] must be polled to receive responses.
pub struct SplitPubSubSink<C = Pin<Box<dyn AsyncStream + Send + Sync>>> {
    sink: SplitSink<Framed<C, ValueCodec>, Vec<u8>>,
    resp_rx: mpsc::Receiver<Value>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl<C> Drop for SplitPubSubSink<C> {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.take().map(|tx| tx.send(()));
    }
}

impl<C> SplitPubSubSink<C>
where
    C: Unpin + AsyncRead + AsyncWrite + Send,
{
    // Create a [`SplitPubSubSink`] from a [`Connection`].
    fn new(framed: Framed<C, ValueCodec>) -> (SplitPubSubSink<C>, SplitPubSubStream<C>) {
        let (sink, stream) = framed.split();
        let (resp_tx, resp_rx) = channel::<Value>(1);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        (
            Self {
                sink,
                resp_rx,
                shutdown_tx: Some(shutdown_tx),
            },
            SplitPubSubStream::new(stream, shutdown_rx, resp_tx),
        )
    }

    // Deliver a command to this pubsub connection.
    async fn query<'a, T>(&'a mut self, cmd: &'a Cmd) -> RedisResult<T>
    where
        T: FromRedisValue,
    {
        let mut out = Vec::new();

        cmd.write_packed_command(&mut out);
        self.sink.send(out).await?;

        self.resp_rx
            .next()
            .await
            .ok_or(next_response_err())
            .and_then(|value| from_redis_value(&value))
    }

    /// Subscribes to a new channel.
    pub async fn subscribe<T: ToRedisArgs>(&mut self, channel: T) -> RedisResult<()> {
        self.query(cmd("SUBSCRIBE").arg(channel)).await
    }

    /// Subscribes to a new channel with a pattern.
    pub async fn psubscribe<T: ToRedisArgs>(&mut self, pchannel: T) -> RedisResult<()> {
        self.query(cmd("PSUBSCRIBE").arg(pchannel)).await
    }

    /// Unsubscribes from a channel.
    pub async fn unsubscribe<T: ToRedisArgs>(&mut self, channel: T) -> RedisResult<()> {
        self.query(cmd("UNSUBSCRIBE").arg(channel)).await
    }

    /// Unsubscribes from a channel with a pattern.
    pub async fn punsubscribe<T: ToRedisArgs>(&mut self, pchannel: T) -> RedisResult<()> {
        self.query(cmd("PUNSUBSCRIBE").arg(pchannel)).await
    }
}

pin_project! {
    /// Represents the receiving side of a pubsub connection.
    /// If the corresponding [`SplitPubSubSink`] is dropped, the stream will exhaust.
    #[project = SplitPubSubStreamProj]
    pub struct SplitPubSubStream<C = Pin<Box<dyn AsyncStream + Send + Sync>>> {
        #[pin]
        stream: SplitStream<Framed<C, ValueCodec>>,
        #[pin]
        shutdown_rx: oneshot::Receiver<()>,
        #[pin]
        resp_tx: mpsc::Sender<Value>,
        resp_waiting: Option<Value>,
    }
}

impl<C> SplitPubSubStream<C>
where
    C: Unpin + AsyncRead + Send,
{
    fn new(
        stream: SplitStream<Framed<C, ValueCodec>>,
        shutdown_rx: oneshot::Receiver<()>,
        resp_tx: mpsc::Sender<Value>,
    ) -> Self {
        SplitPubSubStream {
            stream,
            shutdown_rx,
            resp_tx,
            resp_waiting: None,
        }
    }
}

fn map_response_err<T>(value: Result<T, SendError>) -> Result<T, RedisError> {
    value.map_err(|_| {
        RedisError::from((
            ErrorKind::ResponseForwardError,
            "Response failed to forward",
        ))
    })
}

impl Stream for SplitPubSubStream {
    type Item = Result<Msg, RedisError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let SplitPubSubStreamProj {
            mut stream,
            mut shutdown_rx,
            mut resp_tx,
            resp_waiting,
        } = self.project();

        loop {
            if shutdown_rx.as_mut().poll(cx).is_ready() {
                return std::task::Poll::Ready(None);
            }

            if resp_waiting.is_some() {
                map_response_err(ready!(resp_tx.as_mut().poll_ready(cx)))?;

                map_response_err(resp_tx.as_mut().start_send(resp_waiting.take().unwrap()))?;
            }

            match ready!(stream.as_mut().poll_next(cx)) {
                Some(Ok(Ok(value))) => match Msg::from_value(&value) {
                    Some(msg) => return std::task::Poll::Ready(Some(Ok(msg))),
                    _ => *resp_waiting = Some(value),
                },
                Some(Err(err)) | Some(Ok(Err(err))) => {
                    return std::task::Poll::Ready(Some(Err(err)));
                }
                None => return std::task::Poll::Ready(None),
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

pin_project! {
    /// Represents a pubsub connection.
    #[project = MonitorProj]
    pub struct Monitor<C = Pin<Box<dyn AsyncStream + Send + Sync>>> {
        #[pin]
        framed: Framed<C, ValueCodec>,
        queue: VecDeque<Value>,
    }
}

impl<C> Monitor<C>
where
    C: Unpin + AsyncRead + AsyncWrite + Send,
{
    fn new(con: Connection<C>) -> Monitor<C> {
        let framed = ValueCodec::default().framed(con.con);

        Monitor {
            framed,
            queue: Default::default(),
        }
    }

    /// Query the connection with a command and retrieves the response.
    pub async fn query<'a, T>(&'a mut self, cmd: &'a Cmd) -> RedisResult<T>
    where
        T: FromRedisValue,
    {
        let mut out = Vec::new();

        cmd.write_packed_command(&mut out);
        self.framed.send(out).await?;

        self.next_response().await
    }

    async fn next_response<T>(&mut self) -> RedisResult<T>
    where
        T: FromRedisValue,
    {
        while let Some(item) = self.framed.next().await {
            match item {
                Ok(Ok(value)) => {
                    if Msg::from_value(&value).is_some() {
                        self.queue.push_front(value);
                    } else {
                        return from_redis_value(&value);
                    }
                }
                _ => break,
            }
        }

        Err(next_response_err())
    }

    /// Deliver the `MONITOR` command to this [`Monitor`].
    pub async fn monitor(&mut self) -> RedisResult<()> {
        self.query(&cmd("MONITOR")).await
    }
}

impl Stream for Monitor {
    type Item = Result<Value, RedisError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let MonitorProj { mut framed, queue } = self.project();

        if let Some(msg) = queue.pop_back() {
            return std::task::Poll::Ready(Some(Ok(msg)));
        }

        match ready!(framed.as_mut().poll_next(cx)) {
            Some(Ok(Ok(value))) => std::task::Poll::Ready(Some(Ok(value))),
            Some(Err(err)) | Some(Ok(Err(err))) => std::task::Poll::Ready(Some(Err(err))),
            None => std::task::Poll::Ready(None),
        }
    }
}

async fn get_socket_addrs(
    host: &str,
    port: u16,
) -> RedisResult<impl Iterator<Item = SocketAddr> + Send + '_> {
    #[cfg(feature = "tokio-comp")]
    let socket_addrs = lookup_host((host, port)).await?;
    #[cfg(all(not(feature = "tokio-comp"), feature = "async-std-comp"))]
    let socket_addrs = (host, port).to_socket_addrs().await?;

    let mut socket_addrs = socket_addrs.peekable();
    match socket_addrs.peek() {
        Some(_) => Ok(socket_addrs),
        None => Err(RedisError::from((
            ErrorKind::InvalidClientConfig,
            "No address found for host",
        ))),
    }
}

pub(crate) async fn connect_simple<T: RedisRuntime>(
    connection_info: &ConnectionInfo,
) -> RedisResult<T> {
    Ok(match connection_info.addr {
        ConnectionAddr::Tcp(ref host, port) => {
            let socket_addrs = get_socket_addrs(host, port).await?;
            select_ok(socket_addrs.map(<T>::connect_tcp)).await?.0
        }

        #[cfg(any(feature = "tls-native-tls", feature = "tls-rustls"))]
        ConnectionAddr::TcpTls {
            ref host,
            port,
            insecure,
        } => {
            let socket_addrs = get_socket_addrs(host, port).await?;
            select_ok(
                socket_addrs.map(|socket_addr| <T>::connect_tcp_tls(host, socket_addr, insecure)),
            )
            .await?
            .0
        }

        #[cfg(not(any(feature = "tls-native-tls", feature = "tls-rustls")))]
        ConnectionAddr::TcpTls { .. } => {
            fail!((
                ErrorKind::InvalidClientConfig,
                "Cannot connect to TCP with TLS without the tls feature"
            ));
        }

        #[cfg(unix)]
        ConnectionAddr::Unix(ref path) => <T>::connect_unix(path).await?,

        #[cfg(not(unix))]
        ConnectionAddr::Unix(_) => {
            return Err(RedisError::from((
                ErrorKind::InvalidClientConfig,
                "Cannot connect to unix sockets \
                 on this platform",
            )))
        }
    })
}
