use crate::prelude::Query;

#[derive(Clone, Debug)]
pub struct Subscription {
    pub query: Query,
}
