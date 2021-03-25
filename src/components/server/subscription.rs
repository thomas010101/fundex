use async_trait::async_trait;

#[async_trait]
pub trait SubscriptionServer {
    async fn serve(self, port: u16);
}
