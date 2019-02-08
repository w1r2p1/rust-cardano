use crate::gen::{self, node::client as gen_client};

use chain_core::property::{Block, BlockDate, BlockId, Deserialize, HasHeader, Header, Serialize};
use network_core::client::{
    self as core_client,
    block::{BlockService, HeaderService},
};

use futures::future::Executor;
use tokio::io;
use tokio::prelude::*;
use tower_grpc::{BoxBody, Request, Streaming};
use tower_h2::client::{Background, Connect, ConnectError, Connection};
use tower_util::MakeService;

use std::{
    error,
    fmt::{self, Debug},
    marker::PhantomData,
    str::FromStr,
};

/// gRPC client for blockchain node.
///
/// This type encapsulates the gRPC protocol client that can
/// make connections and perform requests towards other blockchain nodes.
pub struct Client<S, E> {
    node: gen_client::Node<Connection<S, E, BoxBody>>,
}

impl<S, E> Client<S, E>
where
    S: AsyncRead + AsyncWrite,
    E: Executor<Background<S, BoxBody>> + Clone,
{
    pub fn connect<P>(peer: P, executor: E) -> impl Future<Item = Self, Error = Error>
    where
        P: tokio_connect::Connect<Connected = S, Error = io::Error> + 'static,
    {
        let mut make_client = Connect::new(peer, Default::default(), executor);
        make_client
            .make_service(())
            .map_err(|e| Error::Connect(e))
            .map(|conn| {
                // TODO: add origin URL with add_origin middleware from tower-http

                Client {
                    node: gen_client::Node::new(conn),
                }
            })
    }
}

type GrpcFuture<R> = tower_grpc::client::unary::ResponseFuture<
    R,
    tower_h2::client::ResponseFuture,
    tower_h2::RecvBody,
>;

type GrpcStreamFuture<R> =
    tower_grpc::client::server_streaming::ResponseFuture<R, tower_h2::client::ResponseFuture>;

type GrpcError = tower_grpc::Error<tower_h2::client::Error>;

type GrpcStreamError = tower_grpc::Error<()>;

pub struct ResponseFuture<T, R> {
    state: unary_future::State<T, R>,
}

impl<T, R> ResponseFuture<T, R> {
    fn new(future: GrpcFuture<R>) -> Self {
        ResponseFuture {
            state: unary_future::State::Pending(future),
        }
    }
}

pub struct ResponseStreamFuture<T, R> {
    state: stream_future::State<T, R>,
}

impl<T, R> ResponseStreamFuture<T, R> {
    fn new(future: GrpcStreamFuture<R>) -> Self {
        ResponseStreamFuture {
            state: stream_future::State::Pending(future),
        }
    }
}

pub struct ResponseStream<T, R> {
    inner: Streaming<R, tower_h2::RecvBody>,
    _phantom: PhantomData<T>,
}

fn convert_error<T>(e: tower_grpc::Error<T>) -> core_client::Error
where
    T: Debug + Send + Sync + 'static,
{
    core_client::Error::new(core_client::ErrorKind::Rpc, e)
}

pub trait ConvertResponse<T> {
    fn convert_response(self) -> Result<T, core_client::Error>;
}

mod unary_future {
    use super::{
        convert_error, core_client, ConvertResponse, GrpcError, GrpcFuture, ResponseFuture,
    };
    use futures::prelude::*;
    use std::marker::PhantomData;
    use tower_grpc::Response;

    fn poll_and_convert_response<T, R, F>(future: &mut F) -> Poll<T, core_client::Error>
    where
        F: Future<Item = Response<R>, Error = GrpcError>,
        R: ConvertResponse<T>,
    {
        match future.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(res)) => {
                let item = res.into_inner().convert_response()?;
                Ok(Async::Ready(item))
            }
            Err(e) => Err(convert_error(e)),
        }
    }

    pub enum State<T, R> {
        Pending(GrpcFuture<R>),
        Finished(PhantomData<T>),
    }

    impl<T, R> Future for ResponseFuture<T, R>
    where
        R: prost::Message + Default + ConvertResponse<T>,
    {
        type Item = T;
        type Error = core_client::Error;

        fn poll(&mut self) -> Poll<T, core_client::Error> {
            if let State::Pending(ref mut f) = self.state {
                let res = poll_and_convert_response(f);
                if let Ok(Async::NotReady) = res {
                    return Ok(Async::NotReady);
                }
                self.state = State::Finished(PhantomData);
                res
            } else {
                match self.state {
                    State::Pending(_) => unreachable!(),
                    State::Finished(_) => panic!("polled a finished response"),
                }
            }
        }
    }
}

mod stream_future {
    use super::{
        convert_error, core_client, GrpcError, GrpcStreamFuture, ResponseStream,
        ResponseStreamFuture,
    };
    use futures::prelude::*;
    use std::marker::PhantomData;
    use tower_grpc::{Response, Streaming};

    fn poll_and_convert_response<T, R, F>(
        future: &mut F,
    ) -> Poll<ResponseStream<T, R>, core_client::Error>
    where
        F: Future<Item = Response<Streaming<R, tower_h2::RecvBody>>, Error = GrpcError>,
    {
        match future.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(res)) => {
                let stream = ResponseStream {
                    inner: res.into_inner(),
                    _phantom: PhantomData,
                };
                Ok(Async::Ready(stream))
            }
            Err(e) => Err(convert_error(e)),
        }
    }

    pub enum State<T, R> {
        Pending(GrpcStreamFuture<R>),
        Finished(PhantomData<T>),
    }

    impl<T, R> Future for ResponseStreamFuture<T, R>
    where
        R: prost::Message + Default,
    {
        type Item = ResponseStream<T, R>;
        type Error = core_client::Error;

        fn poll(&mut self) -> Poll<ResponseStream<T, R>, core_client::Error> {
            if let State::Pending(ref mut f) = self.state {
                let res = poll_and_convert_response(f);
                if let Ok(Async::NotReady) = res {
                    return Ok(Async::NotReady);
                }
                self.state = State::Finished(PhantomData);
                res
            } else {
                match self.state {
                    State::Pending(_) => unreachable!(),
                    State::Finished(_) => panic!("polled a finished response"),
                }
            }
        }
    }
}

mod stream {
    use super::{convert_error, core_client, ConvertResponse, GrpcStreamError, ResponseStream};
    use futures::prelude::*;

    fn poll_and_convert_item<T, S, R>(stream: &mut S) -> Poll<Option<T>, core_client::Error>
    where
        S: Stream<Item = R, Error = GrpcStreamError>,
        R: ConvertResponse<T>,
    {
        match stream.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(None)) => Ok(Async::Ready(None)),
            Ok(Async::Ready(Some(item))) => {
                let item = item.convert_response()?;
                Ok(Async::Ready(Some(item)))
            }
            Err(e) => Err(convert_error(e)),
        }
    }

    impl<T, R> Stream for ResponseStream<T, R>
    where
        R: prost::Message + Default + ConvertResponse<T>,
    {
        type Item = T;
        type Error = core_client::Error;

        fn poll(&mut self) -> Poll<Option<T>, core_client::Error> {
            poll_and_convert_item(&mut self.inner)
        }
    }
}

fn deserialize_bytes<T>(mut buf: &[u8]) -> Result<T, core_client::Error>
where
    T: Deserialize,
    T::Error: Send + Sync + 'static,
{
    T::deserialize(&mut buf).map_err(|e| core_client::Error::new(core_client::ErrorKind::Format, e))
}

fn parse_str<T>(s: &str) -> Result<T, core_client::Error>
where
    T: FromStr,
    T::Err: error::Error + Send + Sync + 'static,
{
    T::from_str(s).map_err(|e| core_client::Error::new(core_client::ErrorKind::Format, e))
}

fn serialize_to_vec<T>(values: &[T]) -> Vec<Vec<u8>>
where
    T: Serialize,
{
    values
        .iter()
        .map(|x| {
            let mut v = Vec::new();
            x.serialize(&mut v).unwrap();
            v
        })
        .collect()
}

impl<I, D> ConvertResponse<(I, D)> for gen::node::TipResponse
where
    I: BlockId + Deserialize,
    D: BlockDate + FromStr,
    <I as Deserialize>::Error: Send + Sync + 'static,
    <D as FromStr>::Err: error::Error + Send + Sync + 'static,
{
    fn convert_response(self) -> Result<(I, D), core_client::Error> {
        let id = deserialize_bytes(&self.id)?;
        let blockdate = parse_str(&self.blockdate)?;
        Ok((id, blockdate))
    }
}

impl<T> ConvertResponse<T> for gen::node::Block
where
    T: Block,
    <T as Deserialize>::Error: Send + Sync + 'static,
{
    fn convert_response(self) -> Result<T, core_client::Error> {
        let block = deserialize_bytes(&self.content)?;
        Ok(block)
    }
}

impl<T> ConvertResponse<T> for gen::node::Header
where
    T: Header,
    <T as Deserialize>::Error: Send + Sync + 'static,
{
    fn convert_response(self) -> Result<T, core_client::Error> {
        let block = deserialize_bytes(&self.content)?;
        Ok(block)
    }
}

impl<T, S, E> BlockService<T> for Client<S, E>
where
    T: Block,
    S: AsyncRead + AsyncWrite,
    E: Executor<Background<S, BoxBody>> + Clone,
    T::Date: FromStr,
    <T as Deserialize>::Error: Send + Sync + 'static,
    <T::Id as Deserialize>::Error: Send + Sync + 'static,
    <T::Date as FromStr>::Err: error::Error + Send + Sync + 'static,
{
    type TipFuture = ResponseFuture<(T::Id, T::Date), gen::node::TipResponse>;

    type PullBlocksToTipStream = ResponseStream<T, gen::node::Block>;
    type PullBlocksToTipFuture = ResponseStreamFuture<T, gen::node::Block>;

    type GetBlocksStream = ResponseStream<T, gen::node::Block>;
    type GetBlocksFuture = ResponseStreamFuture<T, gen::node::Block>;

    fn tip(&mut self) -> Self::TipFuture {
        let req = gen::node::TipRequest {};
        let future = self.node.tip(Request::new(req));
        ResponseFuture::new(future)
    }

    fn pull_blocks_to_tip(&mut self, from: &[T::Id]) -> Self::PullBlocksToTipFuture {
        let from = serialize_to_vec(from);
        let req = gen::node::PullBlocksToTipRequest { from };
        let future = self.node.pull_blocks_to_tip(Request::new(req));
        ResponseStreamFuture::new(future)
    }
}

impl<T, S, E> HeaderService<T> for Client<S, E>
where
    T: Block + HasHeader,
    S: AsyncRead + AsyncWrite,
    E: Executor<Background<S, BoxBody>> + Clone,
    <T::Header as Deserialize>::Error: Send + Sync + 'static,
{
    //type GetHeadersStream = ResponseStream<T::Header, gen::node::Header>;
    //type GetHeadersFuture = ResponseStreamFuture<T::Header, gen::node::Header>;

    type GetTipFuture = ResponseFuture<T::Header, gen::node::Header>;

    fn tip_header(&mut self) -> Self::GetTipFuture {
        unimplemented!()
    }
}

/// The error type for gRPC client operations.
#[derive(Debug)]
pub enum Error {
    Connect(ConnectError<io::Error>),
}

impl From<ConnectError<io::Error>> for Error {
    fn from(err: ConnectError<io::Error>) -> Self {
        Error::Connect(err)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Connect(e) => write!(f, "connection error: {}", e),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Connect(e) => Some(e),
        }
    }
}
