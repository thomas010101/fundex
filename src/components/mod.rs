use futures::prelude::*;

/// Components dealing with subgraphs.
pub mod sub;

/// Components dealing with Ethereum.
pub mod ethereum;

pub mod arweave;

pub mod three_box;

/// Components dealing with processing GraphQL.
pub mod graphql;

/// Components powering GraphQL, JSON-RPC, WebSocket APIs, Metrics.
pub mod server;

/// Components dealing with storing entities.
pub mod store;

pub mod link_resolver;

/// Components dealing with collecting metrics
pub mod metrics;

/// A component that receives events of type `T`.
pub trait EventConsumer<E> {
    /// Get the event sink.
    ///
    /// Avoid calling directly, prefer helpers such as `forward`.
    fn event_sink(&self) -> Box<dyn Sink<SinkItem = E, SinkError = ()> + Send>;
}

/// A component that outputs events of type `T`.
pub trait EventProducer<E> {
    /// Get the event stream. Because we use single-consumer semantics, the
    /// first caller will take the output stream and any further calls will
    /// return `None`.
    ///
    /// Avoid calling directly, prefer helpers such as `forward`.
    fn take_event_stream(&mut self) -> Option<Box<dyn Stream<Item = E, Error = ()> + Send>>;
}
