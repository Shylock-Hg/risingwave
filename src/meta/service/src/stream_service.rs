// Copyright 2025 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{HashMap, HashSet};

use itertools::Itertools;
use risingwave_common::catalog::{DatabaseId, TableId};
use risingwave_common::secret::LocalSecretManager;
use risingwave_common::util::stream_graph_visitor::visit_stream_node_mut;
use risingwave_connector::source::SplitMetaData;
use risingwave_meta::barrier::BarrierManagerRef;
use risingwave_meta::controller::fragment::StreamingJobInfo;
use risingwave_meta::controller::utils::FragmentDesc;
use risingwave_meta::manager::MetadataManager;
use risingwave_meta::model::ActorId;
use risingwave_meta::stream::{SourceManagerRunningInfo, ThrottleConfig};
use risingwave_meta::{MetaError, model};
use risingwave_meta_model::{FragmentId, ObjectId, SinkId, SourceId, StreamingParallelism};
use risingwave_pb::meta::alter_connector_props_request::AlterConnectorPropsObject;
use risingwave_pb::meta::cancel_creating_jobs_request::Jobs;
use risingwave_pb::meta::list_actor_splits_response::FragmentType;
use risingwave_pb::meta::list_table_fragments_response::{
    ActorInfo, FragmentInfo, TableFragmentInfo,
};
use risingwave_pb::meta::stream_manager_service_server::StreamManagerService;
use risingwave_pb::meta::table_fragments::PbState;
use risingwave_pb::meta::table_fragments::actor_status::PbActorState;
use risingwave_pb::meta::table_fragments::fragment::PbFragmentDistributionType;
use risingwave_pb::meta::*;
use risingwave_pb::stream_plan::stream_node::NodeBody;
use tonic::{Request, Response, Status};

use crate::barrier::{BarrierScheduler, Command};
use crate::manager::MetaSrvEnv;
use crate::stream::GlobalStreamManagerRef;

pub type TonicResponse<T> = Result<Response<T>, Status>;

#[derive(Clone)]
pub struct StreamServiceImpl {
    env: MetaSrvEnv,
    barrier_scheduler: BarrierScheduler,
    barrier_manager: BarrierManagerRef,
    stream_manager: GlobalStreamManagerRef,
    metadata_manager: MetadataManager,
}

impl StreamServiceImpl {
    pub fn new(
        env: MetaSrvEnv,
        barrier_scheduler: BarrierScheduler,
        barrier_manager: BarrierManagerRef,
        stream_manager: GlobalStreamManagerRef,
        metadata_manager: MetadataManager,
    ) -> Self {
        StreamServiceImpl {
            env,
            barrier_scheduler,
            barrier_manager,
            stream_manager,
            metadata_manager,
        }
    }
}

#[async_trait::async_trait]
impl StreamManagerService for StreamServiceImpl {
    async fn flush(&self, request: Request<FlushRequest>) -> TonicResponse<FlushResponse> {
        self.env.idle_manager().record_activity();
        let req = request.into_inner();

        let version_id = self.barrier_scheduler.flush(req.database_id.into()).await?;
        Ok(Response::new(FlushResponse {
            status: None,
            hummock_version_id: version_id.to_u64(),
        }))
    }

    async fn pause(&self, _: Request<PauseRequest>) -> Result<Response<PauseResponse>, Status> {
        for database_id in self.metadata_manager.list_active_database_ids().await? {
            self.barrier_scheduler
                .run_command(database_id, Command::pause())
                .await?;
        }
        Ok(Response::new(PauseResponse {}))
    }

    async fn resume(&self, _: Request<ResumeRequest>) -> Result<Response<ResumeResponse>, Status> {
        for database_id in self.metadata_manager.list_active_database_ids().await? {
            self.barrier_scheduler
                .run_command(database_id, Command::resume())
                .await?;
        }
        Ok(Response::new(ResumeResponse {}))
    }

    async fn apply_throttle(
        &self,
        request: Request<ApplyThrottleRequest>,
    ) -> Result<Response<ApplyThrottleResponse>, Status> {
        let request = request.into_inner();

        let actor_to_apply = match request.kind() {
            ThrottleTarget::Source | ThrottleTarget::TableWithSource => {
                self.metadata_manager
                    .update_source_rate_limit_by_source_id(request.id as SourceId, request.rate)
                    .await?
            }
            ThrottleTarget::Mv => {
                self.metadata_manager
                    .update_backfill_rate_limit_by_table_id(TableId::from(request.id), request.rate)
                    .await?
            }
            ThrottleTarget::CdcTable => {
                self.metadata_manager
                    .update_backfill_rate_limit_by_table_id(TableId::from(request.id), request.rate)
                    .await?
            }
            ThrottleTarget::TableDml => {
                self.metadata_manager
                    .update_dml_rate_limit_by_table_id(TableId::from(request.id), request.rate)
                    .await?
            }
            ThrottleTarget::Sink => {
                self.metadata_manager
                    .update_sink_rate_limit_by_sink_id(request.id as SinkId, request.rate)
                    .await?
            }
            ThrottleTarget::Fragment => {
                self.metadata_manager
                    .update_fragment_rate_limit_by_fragment_id(request.id as _, request.rate)
                    .await?
            }
            ThrottleTarget::Unspecified => {
                return Err(Status::invalid_argument("unspecified throttle target"));
            }
        };

        let request_id = if request.kind() == ThrottleTarget::Fragment {
            self.metadata_manager
                .catalog_controller
                .get_fragment_streaming_job_id(request.id as _)
                .await?
        } else {
            request.id as _
        };

        let database_id = self
            .metadata_manager
            .catalog_controller
            .get_object_database_id(request_id as ObjectId)
            .await?;
        let database_id = DatabaseId::new(database_id as _);
        // TODO: check whether shared source is correct
        let mutation: ThrottleConfig = actor_to_apply
            .iter()
            .map(|(fragment_id, actors)| {
                (
                    *fragment_id,
                    actors
                        .iter()
                        .map(|actor_id| (*actor_id, request.rate))
                        .collect::<HashMap<ActorId, Option<u32>>>(),
                )
            })
            .collect();
        let _i = self
            .barrier_scheduler
            .run_command(database_id, Command::Throttle(mutation))
            .await?;

        Ok(Response::new(ApplyThrottleResponse { status: None }))
    }

    async fn cancel_creating_jobs(
        &self,
        request: Request<CancelCreatingJobsRequest>,
    ) -> TonicResponse<CancelCreatingJobsResponse> {
        let req = request.into_inner();
        let table_ids = match req.jobs.unwrap() {
            Jobs::Infos(infos) => self
                .metadata_manager
                .catalog_controller
                .find_creating_streaming_job_ids(infos.infos)
                .await?
                .into_iter()
                .map(|id| id as _)
                .collect(),
            Jobs::Ids(jobs) => jobs.job_ids,
        };

        let canceled_jobs = self
            .stream_manager
            .cancel_streaming_jobs(table_ids.into_iter().map(TableId::from).collect_vec())
            .await
            .into_iter()
            .map(|id| id.table_id)
            .collect_vec();
        Ok(Response::new(CancelCreatingJobsResponse {
            status: None,
            canceled_jobs,
        }))
    }

    async fn list_table_fragments(
        &self,
        request: Request<ListTableFragmentsRequest>,
    ) -> Result<Response<ListTableFragmentsResponse>, Status> {
        let req = request.into_inner();
        let table_ids = HashSet::<u32>::from_iter(req.table_ids);

        let mut info = HashMap::new();
        for job_id in table_ids {
            let table_fragments = self
                .metadata_manager
                .catalog_controller
                .get_job_fragments_by_id(job_id as _)
                .await?;
            let mut dispatchers = self
                .metadata_manager
                .catalog_controller
                .get_fragment_actor_dispatchers(
                    table_fragments.fragment_ids().map(|id| id as _).collect(),
                )
                .await?;
            let ctx = table_fragments.ctx.to_protobuf();
            info.insert(
                table_fragments.stream_job_id().table_id,
                TableFragmentInfo {
                    fragments: table_fragments
                        .fragments
                        .into_iter()
                        .map(|(id, fragment)| FragmentInfo {
                            id,
                            actors: fragment
                                .actors
                                .into_iter()
                                .map(|actor| ActorInfo {
                                    id: actor.actor_id,
                                    node: Some(fragment.nodes.clone()),
                                    dispatcher: dispatchers
                                        .get_mut(&(fragment.fragment_id as _))
                                        .and_then(|dispatchers| {
                                            dispatchers.remove(&(actor.actor_id as _))
                                        })
                                        .unwrap_or_default(),
                                })
                                .collect_vec(),
                        })
                        .collect_vec(),
                    ctx: Some(ctx),
                },
            );
        }

        Ok(Response::new(ListTableFragmentsResponse {
            table_fragments: info,
        }))
    }

    async fn list_streaming_job_states(
        &self,
        _request: Request<ListStreamingJobStatesRequest>,
    ) -> Result<Response<ListStreamingJobStatesResponse>, Status> {
        let job_infos = self
            .metadata_manager
            .catalog_controller
            .list_streaming_job_infos()
            .await?;
        let states = job_infos
            .into_iter()
            .map(
                |StreamingJobInfo {
                     job_id,
                     job_status,
                     name,
                     parallelism,
                     max_parallelism,
                     resource_group,
                     database_id,
                     schema_id,
                     ..
                 }| {
                    let parallelism = match parallelism {
                        StreamingParallelism::Adaptive => model::TableParallelism::Adaptive,
                        StreamingParallelism::Custom => model::TableParallelism::Custom,
                        StreamingParallelism::Fixed(n) => model::TableParallelism::Fixed(n as _),
                    };

                    list_streaming_job_states_response::StreamingJobState {
                        table_id: job_id as _,
                        name,
                        state: PbState::from(job_status) as _,
                        parallelism: Some(parallelism.into()),
                        max_parallelism: max_parallelism as _,
                        resource_group,
                        database_id: database_id as _,
                        schema_id: schema_id as _,
                    }
                },
            )
            .collect_vec();

        Ok(Response::new(ListStreamingJobStatesResponse { states }))
    }

    async fn list_fragment_distribution(
        &self,
        _request: Request<ListFragmentDistributionRequest>,
    ) -> Result<Response<ListFragmentDistributionResponse>, Status> {
        let fragment_descs = self
            .metadata_manager
            .catalog_controller
            .list_fragment_descs()
            .await?;
        let distributions = fragment_descs
            .into_iter()
            .map(|(fragment_desc, upstreams)| {
                fragment_desc_to_distribution(fragment_desc, upstreams)
            })
            .collect_vec();

        Ok(Response::new(ListFragmentDistributionResponse {
            distributions,
        }))
    }

    async fn list_creating_fragment_distribution(
        &self,
        _request: Request<ListCreatingFragmentDistributionRequest>,
    ) -> Result<Response<ListCreatingFragmentDistributionResponse>, Status> {
        let fragment_descs = self
            .metadata_manager
            .catalog_controller
            .list_creating_fragment_descs()
            .await?;
        let distributions = fragment_descs
            .into_iter()
            .map(|(fragment_desc, upstreams)| {
                fragment_desc_to_distribution(fragment_desc, upstreams)
            })
            .collect_vec();

        Ok(Response::new(ListCreatingFragmentDistributionResponse {
            distributions,
        }))
    }

    async fn get_fragment_by_id(
        &self,
        request: Request<GetFragmentByIdRequest>,
    ) -> Result<Response<GetFragmentByIdResponse>, Status> {
        let req = request.into_inner();
        let fragment_desc = self
            .metadata_manager
            .catalog_controller
            .get_fragment_desc_by_id(req.fragment_id as i32)
            .await?;
        let distribution =
            fragment_desc.map(|(desc, upstreams)| fragment_desc_to_distribution(desc, upstreams));
        Ok(Response::new(GetFragmentByIdResponse { distribution }))
    }

    async fn list_actor_states(
        &self,
        _request: Request<ListActorStatesRequest>,
    ) -> Result<Response<ListActorStatesResponse>, Status> {
        let actor_locations = self
            .metadata_manager
            .catalog_controller
            .list_actor_locations()
            .await?;
        let states = actor_locations
            .into_iter()
            .map(|actor_location| list_actor_states_response::ActorState {
                actor_id: actor_location.actor_id as _,
                fragment_id: actor_location.fragment_id as _,
                state: PbActorState::from(actor_location.status) as _,
                worker_id: actor_location.worker_id as _,
            })
            .collect_vec();

        Ok(Response::new(ListActorStatesResponse { states }))
    }

    async fn list_object_dependencies(
        &self,
        _request: Request<ListObjectDependenciesRequest>,
    ) -> Result<Response<ListObjectDependenciesResponse>, Status> {
        let dependencies = self
            .metadata_manager
            .catalog_controller
            .list_created_object_dependencies()
            .await?;

        Ok(Response::new(ListObjectDependenciesResponse {
            dependencies,
        }))
    }

    async fn recover(
        &self,
        _request: Request<RecoverRequest>,
    ) -> Result<Response<RecoverResponse>, Status> {
        self.barrier_manager.adhoc_recovery().await?;
        Ok(Response::new(RecoverResponse {}))
    }

    async fn list_actor_splits(
        &self,
        _request: Request<ListActorSplitsRequest>,
    ) -> Result<Response<ListActorSplitsResponse>, Status> {
        let SourceManagerRunningInfo {
            source_fragments,
            backfill_fragments,
            mut actor_splits,
        } = self.stream_manager.source_manager.get_running_info().await;

        let source_actors = self
            .metadata_manager
            .catalog_controller
            .list_source_actors()
            .await?;

        let is_shared_source = self
            .metadata_manager
            .catalog_controller
            .list_source_id_with_shared_types()
            .await?;

        let fragment_to_source: HashMap<_, _> = source_fragments
            .into_iter()
            .flat_map(|(source_id, fragment_ids)| {
                let source_type = if is_shared_source
                    .get(&(source_id as _))
                    .copied()
                    .unwrap_or(false)
                {
                    FragmentType::SharedSource
                } else {
                    FragmentType::NonSharedSource
                };

                fragment_ids
                    .into_iter()
                    .map(move |fragment_id| (fragment_id, (source_id, source_type)))
            })
            .chain(
                backfill_fragments
                    .into_iter()
                    .flat_map(|(source_id, fragment_ids)| {
                        fragment_ids.into_iter().flat_map(
                            move |(fragment_id, upstream_fragment_id)| {
                                [
                                    (fragment_id, (source_id, FragmentType::SharedSourceBackfill)),
                                    (
                                        upstream_fragment_id,
                                        (source_id, FragmentType::SharedSource),
                                    ),
                                ]
                            },
                        )
                    }),
            )
            .collect();

        let actor_splits = source_actors
            .into_iter()
            .flat_map(|(actor_id, fragment_id)| {
                let (source_id, fragment_type) = fragment_to_source
                    .get(&(fragment_id as _))
                    .copied()
                    .unwrap_or_default();

                actor_splits
                    .remove(&(actor_id as _))
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |split| list_actor_splits_response::ActorSplit {
                        actor_id: actor_id as _,
                        source_id: source_id as _,
                        fragment_id: fragment_id as _,
                        split_id: split.id().to_string(),
                        fragment_type: fragment_type.into(),
                    })
            })
            .collect_vec();

        Ok(Response::new(ListActorSplitsResponse { actor_splits }))
    }

    async fn list_rate_limits(
        &self,
        _request: Request<ListRateLimitsRequest>,
    ) -> Result<Response<ListRateLimitsResponse>, Status> {
        let rate_limits = self
            .metadata_manager
            .catalog_controller
            .list_rate_limits()
            .await?;
        Ok(Response::new(ListRateLimitsResponse { rate_limits }))
    }

    #[cfg_attr(coverage, coverage(off))]
    async fn refresh(
        &self,
        request: Request<RefreshRequest>,
    ) -> Result<Response<RefreshResponse>, Status> {
        let req = request.into_inner();

        tracing::info!("Refreshing table with id: {}", req.table_id);

        // Create refresh manager and execute refresh
        let refresh_manager = risingwave_meta::stream::RefreshManager::new(
            self.metadata_manager.clone(),
            self.barrier_scheduler.clone(),
        );

        let response = refresh_manager.refresh_table(req).await?;

        Ok(Response::new(response))
    }

    async fn alter_connector_props(
        &self,
        request: Request<AlterConnectorPropsRequest>,
    ) -> Result<Response<AlterConnectorPropsResponse>, Status> {
        let request = request.into_inner();

        let database_id = self
            .metadata_manager
            .catalog_controller
            .get_object_database_id(request.object_id as ObjectId)
            .await?;
        let database_id = DatabaseId::new(database_id as _);

        let secret_manager = LocalSecretManager::global();
        let new_props_plaintext = match request.object_type() {
            AlterConnectorPropsObject::Sink => {
                self.metadata_manager
                    .update_sink_props_by_sink_id(
                        request.object_id as i32,
                        request.changed_props.clone().into_iter().collect(),
                    )
                    .await?
            }
            AlterConnectorPropsObject::Source => {
                // alter source and table's associated source
                if request.connector_conn_ref.is_some() {
                    return Err(Status::invalid_argument(
                        "alter connector_conn_ref is not supported",
                    ));
                }
                let options_with_secret = self
                    .metadata_manager
                    .catalog_controller
                    .update_source_props_by_source_id(
                        request.object_id as SourceId,
                        request.changed_props.clone().into_iter().collect(),
                        request.changed_secret_refs.clone().into_iter().collect(),
                    )
                    .await?;

                self.stream_manager
                    .source_manager
                    .validate_source_once(request.object_id, options_with_secret.clone())
                    .await?;

                let (options, secret_refs) = options_with_secret.into_parts();
                secret_manager
                    .fill_secrets(options, secret_refs)
                    .map_err(MetaError::from)?
                    .into_iter()
                    .collect()
            }
            AlterConnectorPropsObject::Connection => {
                todo!()
            }
            AlterConnectorPropsObject::Unspecified => unreachable!(),
        };

        let mut mutation = HashMap::default();
        mutation.insert(request.object_id, new_props_plaintext);

        let _i = self
            .barrier_scheduler
            .run_command(database_id, Command::ConnectorPropsChange(mutation))
            .await?;

        Ok(Response::new(AlterConnectorPropsResponse {}))
    }

    async fn set_sync_log_store_aligned(
        &self,
        request: Request<SetSyncLogStoreAlignedRequest>,
    ) -> Result<Response<SetSyncLogStoreAlignedResponse>, Status> {
        let req = request.into_inner();
        let job_id = req.job_id;
        let aligned = req.aligned;

        self.metadata_manager
            .catalog_controller
            .mutate_fragments_by_job_id(
                job_id as _,
                |_mask, stream_node| {
                    let mut visited = false;
                    visit_stream_node_mut(stream_node, |body| {
                        if let NodeBody::SyncLogStore(sync_log_store) = body {
                            sync_log_store.aligned = aligned;
                            visited = true
                        }
                    });
                    Ok(visited)
                },
                "no fragments found with synced log store",
            )
            .await?;

        Ok(Response::new(SetSyncLogStoreAlignedResponse {}))
    }
}

fn fragment_desc_to_distribution(
    fragment_desc: FragmentDesc,
    upstreams: Vec<FragmentId>,
) -> FragmentDistribution {
    FragmentDistribution {
        fragment_id: fragment_desc.fragment_id as _,
        table_id: fragment_desc.job_id as _,
        distribution_type: PbFragmentDistributionType::from(fragment_desc.distribution_type) as _,
        state_table_ids: fragment_desc.state_table_ids.into_u32_array(),
        upstream_fragment_ids: upstreams.iter().map(|id| *id as _).collect(),
        fragment_type_mask: fragment_desc.fragment_type_mask as _,
        parallelism: fragment_desc.parallelism as _,
        vnode_count: fragment_desc.vnode_count as _,
        node: Some(fragment_desc.stream_node.to_protobuf()),
    }
}
