// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A client for replicas of a compute instance.

use std::time::Duration;

use anyhow::bail;
use differential_dataflow::lattice::Lattice;
use timely::progress::Timestamp;
use tokio::select;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use mz_build_info::BuildInfo;
use mz_ore::retry::Retry;
use mz_ore::task::{AbortOnDropHandle, JoinHandleExt};
use mz_service::client::GenericClient;

use crate::logging::LoggingConfig;
use crate::protocol::command::{ComputeCommand, ComputeStartupEpoch, TimelyConfig};
use crate::protocol::response::ComputeResponse;
use crate::service::{ComputeClient, ComputeGrpcClient};

use super::{ComputeReplicaLocation, ReplicaId};

/// Replica-specific configuration.
#[derive(Clone, Debug)]
pub(super) struct ReplicaConfig {
    pub location: ComputeReplicaLocation,
    pub logging: LoggingConfig,
    pub idle_arrangement_merge_effort: u32,
}

/// State for a single replica.
#[derive(Debug)]
pub(super) struct Replica<T> {
    /// A sender for commands for the replica.
    ///
    /// If sending to this channel fails, the replica has failed and requires
    /// rehydration.
    command_tx: UnboundedSender<ComputeCommand<T>>,
    /// A receiver for responses from the replica.
    ///
    /// If receiving from the channel returns `None`, the replica has failed
    /// and requires rehydration.
    response_rx: UnboundedReceiver<ComputeResponse<T>>,
    /// A handle to the task that aborts it when the replica is dropped.
    _task: AbortOnDropHandle<()>,
    /// Configuration specific to this replica.
    pub config: ReplicaConfig,
}

impl<T> Replica<T>
where
    T: Timestamp + Lattice,
    ComputeGrpcClient: ComputeClient<T>,
{
    pub(super) fn spawn(
        id: ReplicaId,
        build_info: &'static BuildInfo,
        config: ReplicaConfig,
        epoch: ComputeStartupEpoch,
    ) -> Self {
        // Launch a task to handle communication with the replica
        // asynchronously. This isolates the main controller thread from
        // the replica.
        let (command_tx, command_rx) = unbounded_channel();
        let (response_tx, response_rx) = unbounded_channel();

        let task = mz_ore::task::spawn(
            || format!("active-replication-replica-{id}"),
            ReplicaTask {
                replica_id: id,
                build_info,
                config: config.clone(),
                command_rx,
                response_tx,
                epoch,
            }
            .run(),
        );

        Self {
            command_tx,
            response_rx,
            _task: task.abort_on_drop(),
            config,
        }
    }

    /// Sends a command to this replica.
    pub(super) fn send(
        &self,
        command: ComputeCommand<T>,
    ) -> Result<(), SendError<ComputeCommand<T>>> {
        self.command_tx.send(command)
    }

    /// Receives the next response from this replica.
    ///
    /// This method is cancellation safe.
    pub(super) async fn recv(&mut self) -> Option<ComputeResponse<T>> {
        self.response_rx.recv().await
    }
}

/// Configuration for `replica_task`.
struct ReplicaTask<T> {
    /// The ID of the replica.
    replica_id: ReplicaId,
    /// Replica configuration.
    config: ReplicaConfig,
    /// The build information for this process.
    build_info: &'static BuildInfo,
    /// A channel upon which commands intended for the replica are delivered.
    command_rx: UnboundedReceiver<ComputeCommand<T>>,
    /// A channel upon which responses from the replica are delivered.
    response_tx: UnboundedSender<ComputeResponse<T>>,
    /// A number (technically, pair of numbers) identifying this incarnation of the replica.
    /// The semantics of this don't matter, except that it must strictly increase.
    epoch: ComputeStartupEpoch,
}

impl<T> ReplicaTask<T>
where
    T: Timestamp + Lattice,
    ComputeGrpcClient: ComputeClient<T>,
{
    /// Asynchronously forwards commands to and responses from a single replica.
    async fn run(self) {
        let ReplicaTask {
            replica_id,
            config,
            build_info,
            command_rx,
            response_tx,
            epoch,
        } = self;

        tracing::info!("starting replica task for {replica_id}");

        let addrs = config.location.computectl_addrs;
        let timely_config = TimelyConfig {
            workers: config.location.workers,
            process: 0,
            addresses: config.location.compute_addrs,
            idle_arrangement_merge_effort: config.idle_arrangement_merge_effort,
        };
        let cmd_spec = CommandSpecialization {
            logging_config: config.logging,
            timely_config,
            epoch,
        };

        let result = run_message_loop(
            replica_id,
            command_rx,
            response_tx,
            build_info,
            addrs,
            cmd_spec,
        )
        .await;

        if let Err(error) = result {
            tracing::warn!("replica task for {replica_id} failed: {error}");
        } else {
            panic!("replica message loop should never return successfully");
        }
    }
}

/// Connects to the replica and runs the message loop.
///
/// The initial replica connection is retried forever (with backoff). Once connected, the task
/// returns (with an `Err`) if it encounters an error condition (e.g. the replica disconnects).
///
/// If no error condition is encountered, the task runs until the controller disconnects from the
/// command channel, or the task is dropped.
async fn run_message_loop<T>(
    replica_id: ReplicaId,
    mut command_rx: UnboundedReceiver<ComputeCommand<T>>,
    response_tx: UnboundedSender<ComputeResponse<T>>,
    build_info: &BuildInfo,
    addrs: Vec<String>,
    cmd_spec: CommandSpecialization,
) -> Result<(), anyhow::Error>
where
    T: Timestamp + Lattice,
    ComputeGrpcClient: ComputeClient<T>,
{
    let mut client = Retry::default()
        .clamp_backoff(Duration::from_secs(32))
        .retry_async(|state| {
            let addrs = addrs.clone();
            let version = build_info.semver_version();

            async move {
                match ComputeGrpcClient::connect_partitioned(addrs, version).await {
                    Ok(client) => Ok(client),
                    Err(e) => {
                        if state.i >= mz_service::retry::INFO_MIN_RETRIES {
                            tracing::info!(
                                "error connecting to replica {replica_id}, retrying in {:?}: {e}",
                                state.next_backoff.unwrap()
                            );
                        } else {
                            tracing::debug!(
                                "error connecting to replica {replica_id}, retrying in {:?}: {e}",
                                state.next_backoff.unwrap()
                            );
                        }
                        Err(e)
                    }
                }
            }
        })
        .await
        .expect("retry retries forever");

    loop {
        select! {
            // Command from controller to forward to replica.
            command = command_rx.recv() => match command {
                None => {
                    // Controller is no longer interested in this replica. Shut down.
                    break;
                }
                Some(mut command) => {
                    cmd_spec.specialize_command(&mut command);
                    client.send(command).await?;
                }
            },
            // Response from replica to forward to controller.
            response = client.recv() => {
                let Some(response) = response? else {
                    bail!("replica unexpectedly gracefully terminated connection");
                };
                if response_tx.send(response).is_err() {
                    // Controller is no longer interested in this replica. Shut down.
                    break;
                }
            }
        }
    }

    Ok(())
}

struct CommandSpecialization {
    logging_config: LoggingConfig,
    timely_config: TimelyConfig,
    epoch: ComputeStartupEpoch,
}

impl CommandSpecialization {
    /// Specialize a command for the given replica configuration.
    ///
    /// Most `ComputeCommand`s are independent of the target replica, but some
    /// contain replica-specific fields that must be adjusted before sending.
    fn specialize_command<T>(&self, command: &mut ComputeCommand<T>) {
        if let ComputeCommand::CreateInstance(logging) = command {
            *logging = self.logging_config.clone();
        }

        if let ComputeCommand::CreateTimely { config, epoch } = command {
            *config = self.timely_config.clone();
            *epoch = self.epoch;
        }
    }
}
