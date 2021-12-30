use risingwave_pb::meta::id_generator_service_server::IdGeneratorService;
use risingwave_pb::meta::{GetIdRequest, GetIdResponse};
use tonic::{Request, Response, Status};

use crate::manager::{IdGeneratorManagerRef, MetaSrvEnv};

#[derive(Clone)]
pub struct IdGeneratorServiceImpl {
    id_gen_manager: IdGeneratorManagerRef,
}

impl IdGeneratorServiceImpl {
    pub fn new(env: MetaSrvEnv) -> Self {
        IdGeneratorServiceImpl {
            id_gen_manager: env.id_gen_manager_ref(),
        }
    }
}

#[async_trait::async_trait]
impl IdGeneratorService for IdGeneratorServiceImpl {
    #[cfg(not(tarpaulin_include))]
    async fn get_id(
        &self,
        request: Request<GetIdRequest>,
    ) -> Result<Response<GetIdResponse>, Status> {
        let req = request.into_inner();
        let category = req.get_category();
        let interval = req.get_interval();
        Ok(Response::new(GetIdResponse {
            status: None,
            id: self
                .id_gen_manager
                .generate_interval(category, interval)
                .await
                .map_err(|e| e.to_grpc_status())?,
        }))
    }
}
