use std::sync::Arc;

use crate::prelude::SubgraphDeploymentId;

#[async_trait::async_trait]
pub trait SubgraphInstanceManager: Send + Sync + 'static {
    async fn start_subgraph(
        self: Arc<Self>,
        id: SubgraphDeploymentId,
        manifest: serde_yaml::Mapping,
    );
    fn stop_subgraph(&self, id: SubgraphDeploymentId);
}
