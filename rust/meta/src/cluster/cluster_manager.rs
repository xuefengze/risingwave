use std::sync::Arc;

use async_trait::async_trait;
use prost::Message;
use risingwave_common::error::Result;
use risingwave_pb::common::Cluster;

use crate::manager::{Config, IdGeneratorManagerRef, MetaSrvEnv, SINGLE_VERSION_EPOCH};
use crate::storage::MetaStoreRef;

#[async_trait]
pub trait ClusterMetaManager: Sync + Send + 'static {
    async fn list_cluster(&self) -> Result<Vec<Cluster>>;
    async fn get_cluster(&self, cluster_id: u32) -> Result<Cluster>;
    async fn put_cluster(&self, cluster: Cluster) -> Result<()>;
    async fn delete_cluster(&self, cluster_id: u32) -> Result<()>;
}

/// [`StoredClusterManager`] manager cluster/worker meta data in [`MetaStore`].
pub struct StoredClusterManager {
    meta_store_ref: MetaStoreRef,
    pub id_gen_manager_ref: IdGeneratorManagerRef,
    config: Arc<Config>,
}

impl StoredClusterManager {
    pub fn new(env: MetaSrvEnv) -> Self {
        Self {
            meta_store_ref: env.meta_store_ref(),
            id_gen_manager_ref: env.id_gen_manager_ref(),
            config: env.config(),
        }
    }
}

#[async_trait]
impl ClusterMetaManager for StoredClusterManager {
    async fn list_cluster(&self) -> Result<Vec<Cluster>> {
        let clusters_pb = self
            .meta_store_ref
            .list_cf(self.config.get_cluster_cf())
            .await?;

        Ok(clusters_pb
            .iter()
            .map(|c| Cluster::decode(c.as_slice()).unwrap())
            .collect::<Vec<_>>())
    }

    async fn get_cluster(&self, cluster_id: u32) -> Result<Cluster> {
        let cluster_pb = self
            .meta_store_ref
            .get_cf(
                self.config.get_cluster_cf(),
                &cluster_id.to_be_bytes(),
                SINGLE_VERSION_EPOCH,
            )
            .await?;

        Ok(Cluster::decode(cluster_pb.as_slice())?)
    }

    async fn put_cluster(&self, cluster: Cluster) -> Result<()> {
        self.meta_store_ref
            .put_cf(
                self.config.get_cluster_cf(),
                &cluster.get_id().to_be_bytes(),
                &cluster.encode_to_vec(),
                SINGLE_VERSION_EPOCH,
            )
            .await
    }

    async fn delete_cluster(&self, cluster_id: u32) -> Result<()> {
        self.meta_store_ref
            .delete_cf(
                self.config.get_cluster_cf(),
                &cluster_id.to_be_bytes(),
                SINGLE_VERSION_EPOCH,
            )
            .await
    }
}

#[cfg(test)]
mod tests {

    use risingwave_pb::common::WorkerNode;

    use super::*;

    #[tokio::test]
    async fn test_cluster_manager() -> Result<()> {
        let cluster_manager = StoredClusterManager::new(MetaSrvEnv::for_test().await);

        assert!(cluster_manager.list_cluster().await.is_ok());
        assert!(cluster_manager.get_cluster(0).await.is_err());

        for i in 0..100 {
            assert!(cluster_manager
                .put_cluster(Cluster {
                    id: i,
                    nodes: vec![WorkerNode {
                        id: i * 2,
                        host: None
                    }],
                    config: Default::default()
                })
                .await
                .is_ok());
        }

        let cluster = cluster_manager.get_cluster(10).await?;
        assert_eq!(cluster.id, 10);
        assert_eq!(cluster.nodes[0].id, 20);
        let clusters = cluster_manager.list_cluster().await?;
        assert_eq!(clusters.len(), 100);

        Ok(())
    }
}
