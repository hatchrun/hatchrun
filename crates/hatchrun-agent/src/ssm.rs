// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! SSM Parameter Store access.

use anyhow::Result;
use aws_sdk_ssm::Client;
use aws_sdk_ssm::operation::get_parameter::GetParameterError;

use crate::source::Parameters;

/// Reads and deletes SSM parameters, decrypting `SecureString` values.
pub struct Ssm {
    client: Client,
}

impl Ssm {
    /// Wraps an SSM client.
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

impl Parameters for Ssm {
    async fn get(&self, name: &str) -> Result<Option<String>> {
        let result = self
            .client
            .get_parameter()
            .name(name)
            .with_decryption(true)
            .send()
            .await;
        match result {
            Ok(output) => Ok(output.parameter.and_then(|parameter| parameter.value)),
            Err(err)
                if err
                    .as_service_error()
                    .is_some_and(GetParameterError::is_parameter_not_found) =>
            {
                Ok(None)
            }
            Err(err) => Err(err.into()),
        }
    }

    async fn delete(&self, name: &str) -> Result<()> {
        self.client.delete_parameter().name(name).send().await?;
        Ok(())
    }
}
