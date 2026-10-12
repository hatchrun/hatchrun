// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Runner instances on EC2.

use std::future::Future;

use anyhow::{Context, Result, bail};
use aws_sdk_ec2::Client;
use aws_sdk_ec2::error::ProvideErrorMetadata;
use aws_sdk_ec2::types::{
    Filter, LaunchTemplateSpecification, ResourceType, ShutdownBehavior, Tag, TagSpecification,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hatchrun_scaler_core::Instance;
use tracing::{info, warn};

/// Tag with the scale set id, which marks the instances this scaler owns.
pub const TAG_SCALE_SET_ID: &str = "hatchrun:scale-set-id";
/// Tag with the scale set name.
pub const TAG_SCALE_SET: &str = "hatchrun:scale-set";
/// Tag with the runner name.
pub const TAG_RUNNER_NAME: &str = "hatchrun:runner-name";
/// Tag with the runner id.
pub const TAG_RUNNER_ID: &str = "hatchrun:runner-id";

/// Errors that mean "try the next subnet".
const CAPACITY_ERRORS: &[&str] = &[
    "InsufficientInstanceCapacity",
    "InsufficientFreeAddressesInSubnet",
    "Unsupported",
    "SpotMaxPriceTooLow",
];

/// Where runners run.
pub trait Cloud {
    /// Starts an instance for the runner with `user_data` for the agent
    /// and returns its id.
    fn launch(
        &self,
        name: &str,
        runner_id: u64,
        user_data: &str,
    ) -> impl Future<Output = Result<String>>;
    /// Lists the scale set's instances that are not terminated.
    fn instances(&self) -> impl Future<Output = Result<Vec<Instance>>>;
    /// Terminates an instance. An instance that is already gone is fine.
    fn terminate(&self, instance_id: &str) -> impl Future<Output = Result<()>>;
}

/// Launches runners from a launch template.
pub struct Ec2 {
    /// The EC2 client.
    pub client: Client,
    /// Launch template id (`lt-...`) or name.
    pub launch_template: String,
    /// Launch template version.
    pub launch_template_version: String,
    /// Subnets to try in order. Empty uses the template's network.
    pub subnets: Vec<String>,
    /// The scale set id.
    pub scale_set_id: u64,
    /// The scale set name.
    pub scale_set_name: String,
}

impl Ec2 {
    fn template(&self) -> LaunchTemplateSpecification {
        let spec = LaunchTemplateSpecification::builder().version(&self.launch_template_version);
        if self.launch_template.starts_with("lt-") {
            spec.launch_template_id(&self.launch_template)
        } else {
            spec.launch_template_name(&self.launch_template)
        }
        .build()
    }

    fn tags(&self, resource: ResourceType, name: &str, runner_id: u64) -> TagSpecification {
        let tag = |key: &str, value: String| Tag::builder().key(key).value(value).build();
        TagSpecification::builder()
            .resource_type(resource)
            .tags(tag("Name", name.to_owned()))
            .tags(tag(TAG_SCALE_SET_ID, self.scale_set_id.to_string()))
            .tags(tag(TAG_SCALE_SET, self.scale_set_name.clone()))
            .tags(tag(TAG_RUNNER_NAME, name.to_owned()))
            .tags(tag(TAG_RUNNER_ID, runner_id.to_string()))
            .build()
    }
}

impl Cloud for Ec2 {
    async fn launch(&self, name: &str, runner_id: u64, user_data: &str) -> Result<String> {
        let user_data = STANDARD.encode(user_data);
        let subnets: Vec<Option<&str>> = if self.subnets.is_empty() {
            vec![None]
        } else {
            self.subnets
                .iter()
                .map(|subnet| Some(subnet.as_str()))
                .collect()
        };

        let mut last_error = None;
        for subnet in subnets {
            let result = self
                .client
                .run_instances()
                .launch_template(self.template())
                .min_count(1)
                .max_count(1)
                .set_subnet_id(subnet.map(str::to_owned))
                .user_data(&user_data)
                .instance_initiated_shutdown_behavior(ShutdownBehavior::Terminate)
                .tag_specifications(self.tags(ResourceType::Instance, name, runner_id))
                .tag_specifications(self.tags(ResourceType::Volume, name, runner_id))
                // Runner names are unique, so a retried call cannot start
                // a second instance.
                .client_token(name)
                .send()
                .await;
            match result {
                Ok(output) => {
                    let instance_id = output
                        .instances()
                        .first()
                        .and_then(|instance| instance.instance_id())
                        .context("RunInstances returned no instance")?
                        .to_owned();
                    info!(runner = name, instance_id, subnet, "launched instance");
                    return Ok(instance_id);
                }
                Err(err)
                    if err
                        .code()
                        .is_some_and(|code| CAPACITY_ERRORS.contains(&code)) =>
                {
                    warn!(
                        runner = name,
                        subnet,
                        "no capacity: {}",
                        err.message().unwrap_or_default()
                    );
                    last_error = Some(err);
                }
                Err(err) => return Err(err).context("launch instance"),
            }
        }
        match last_error {
            Some(err) => Err(err).context("no subnet had capacity"),
            None => bail!("no subnet to launch in"),
        }
    }

    async fn instances(&self) -> Result<Vec<Instance>> {
        let mut pages = self
            .client
            .describe_instances()
            .filters(
                Filter::builder()
                    .name(format!("tag:{TAG_SCALE_SET_ID}"))
                    .values(self.scale_set_id.to_string())
                    .build(),
            )
            .filters(
                Filter::builder()
                    .name("instance-state-name")
                    .set_values(Some(
                        ["pending", "running", "stopping", "stopped"]
                            .map(str::to_owned)
                            .to_vec(),
                    ))
                    .build(),
            )
            .into_paginator()
            .send();

        let mut instances = Vec::new();
        while let Some(page) = pages.next().await {
            let page = page.context("describe instances")?;
            for instance in page
                .reservations()
                .iter()
                .flat_map(aws_sdk_ec2::types::Reservation::instances)
            {
                let tag = |key: &str| {
                    instance
                        .tags()
                        .iter()
                        .find(|tag| tag.key() == Some(key))
                        .and_then(|tag| tag.value())
                };
                let (Some(id), Some(runner_name)) = (instance.instance_id(), tag(TAG_RUNNER_NAME))
                else {
                    continue;
                };
                instances.push(Instance {
                    id: id.to_owned(),
                    runner_name: runner_name.to_owned(),
                    runner_id: tag(TAG_RUNNER_ID).and_then(|id| id.parse().ok()),
                    launched_at: instance
                        .launch_time()
                        .and_then(|time| u64::try_from(time.secs()).ok())
                        .unwrap_or_default(),
                });
            }
        }
        Ok(instances)
    }

    async fn terminate(&self, instance_id: &str) -> Result<()> {
        match self
            .client
            .terminate_instances()
            .instance_ids(instance_id)
            .send()
            .await
        {
            Ok(_) => {
                info!(instance_id, "terminated instance");
                Ok(())
            }
            Err(err) if err.code() == Some("InvalidInstanceID.NotFound") => Ok(()),
            Err(err) => Err(err).with_context(|| format!("terminate {instance_id}")),
        }
    }
}
