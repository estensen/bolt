//! Module for interacting with the Constraints client API via its Builder API interface.
//! The Bolt sidecar's main purpose is to sit between the beacon node and Constraints client,
//! so most requests are simply proxied to its API.

use std::collections::HashSet;

use axum::http::StatusCode;
use beacon_api_client::VersionedValue;
use ethereum_consensus::{
    builder::SignedValidatorRegistration, crypto::PublicKey as BlsPublicKey,
    deneb::mainnet::SignedBlindedBeaconBlock, Fork,
};
use reqwest::Url;
use tracing::error;

use crate::{
    api::{
        builder::GetHeaderParams,
        spec::{
            BuilderApi, BuilderApiError, ConstraintsApi, ErrorResponse, DELEGATE_PATH,
            GET_PAYLOAD_PATH, REGISTER_VALIDATORS_PATH, REVOKE_PATH, STATUS_PATH,
            SUBMIT_CONSTRAINTS_PATH,
        },
    },
    primitives::{
        BatchedSignedConstraints, GetPayloadResponse, SignedBuilderBid, SignedDelegation,
        SignedRevocation,
    },
};

/// A client for interacting with the Constraints client API.
#[derive(Debug, Clone)]
pub struct ConstraintsClient {
    /// The URL of the MEV-Boost target supporting the Constraints API.
    pub url: Url,
    client: reqwest::Client,
    delegations: Vec<SignedDelegation>,
}

impl ConstraintsClient {
    /// Creates a new constraint client with the given URL.
    pub fn new<U: Into<Url>>(url: U) -> Self {
        Self {
            url: url.into(),
            client: reqwest::ClientBuilder::new().user_agent("bolt-sidecar").build().unwrap(),
            delegations: Vec::new(),
        }
    }

    /// Adds a list of delegations to the client.
    pub fn add_delegations(&mut self, delegations: Vec<SignedDelegation>) {
        self.delegations.extend(delegations);
    }

    /// Finds all delegations for the given validator public key.
    pub fn find_delegatees(&self, validator_pubkey: &BlsPublicKey) -> HashSet<BlsPublicKey> {
        self.delegations
            .iter()
            .filter(|d| d.message.validator_pubkey == *validator_pubkey)
            .map(|d| d.message.delegatee_pubkey.clone())
            .collect::<HashSet<_>>()
    }

    fn endpoint(&self, path: &str) -> Url {
        self.url.join(path).unwrap_or_else(|e| {
            error!(err = ?e, "Failed to join path: {} with url: {}", path, self.url);
            self.url.clone()
        })
    }
}

#[async_trait::async_trait]
impl BuilderApi for ConstraintsClient {
    /// Implements: <https://ethereum.github.io/builder-specs/#/Builder/status>
    async fn status(&self) -> Result<StatusCode, BuilderApiError> {
        Ok(self
            .client
            .get(self.endpoint(STATUS_PATH))
            .header("content-type", "application/json")
            .send()
            .await?
            .status())
    }

    /// Implements: <https://ethereum.github.io/builder-specs/#/Builder/registerValidator>
    async fn register_validators(
        &self,
        registrations: Vec<SignedValidatorRegistration>,
    ) -> Result<(), BuilderApiError> {
        let response = self
            .client
            .post(self.endpoint(REGISTER_VALIDATORS_PATH))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&registrations)?)
            .send()
            .await?;

        if response.status() != StatusCode::OK {
            let error = response.json::<ErrorResponse>().await?;
            return Err(BuilderApiError::FailedRegisteringValidators(error));
        }

        // If there are any delegations, propagate the one associated to the incoming registrations to the relay
        if self.delegations.is_empty() {
            return Ok(());
        } else {
            let validator_pubkeys =
                registrations.iter().map(|r| r.message.public_key.clone()).collect::<HashSet<_>>();
            let filtered_delegations = self
                .delegations
                .iter()
                .filter(|d| validator_pubkeys.contains(&d.message.validator_pubkey))
                .cloned()
                .collect::<Vec<_>>();

            if let Err(err) = self.delegate(&filtered_delegations).await {
                error!(?err, "Failed to propagate delegations during validator registration");
            }
        }

        Ok(())
    }

    /// Implements: <https://ethereum.github.io/builder-specs/#/Builder/getHeader>
    async fn get_header(
        &self,
        params: GetHeaderParams,
    ) -> Result<SignedBuilderBid, BuilderApiError> {
        let parent_hash = format!("0x{}", hex::encode(params.parent_hash.as_ref()));
        let public_key = format!("0x{}", hex::encode(params.public_key.as_ref()));

        let response = self
            .client
            .get(self.endpoint(&format!(
                "/eth/v1/builder/header/{}/{}/{}",
                params.slot, parent_hash, public_key
            )))
            .header("content-type", "application/json")
            .send()
            .await?;

        if response.status() != StatusCode::OK {
            let error = response.json::<ErrorResponse>().await?;
            return Err(BuilderApiError::FailedGettingHeader(error));
        }

        let header = response.json::<SignedBuilderBid>().await?;

        Ok(header)
    }

    /// Implements: <https://ethereum.github.io/builder-specs/#/Builder/submitBlindedBlock>
    async fn get_payload(
        &self,
        signed_block: SignedBlindedBeaconBlock,
    ) -> Result<GetPayloadResponse, BuilderApiError> {
        let response = self
            .client
            .post(self.endpoint(GET_PAYLOAD_PATH))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&signed_block)?)
            .send()
            .await?;

        if response.status() != StatusCode::OK {
            let error = response.json::<ErrorResponse>().await?;
            return Err(BuilderApiError::FailedGettingPayload(error));
        }

        let payload = response.json().await?;

        Ok(payload)
    }
}

#[async_trait::async_trait]
impl ConstraintsApi for ConstraintsClient {
    async fn submit_constraints(
        &self,
        constraints: &BatchedSignedConstraints,
    ) -> Result<(), BuilderApiError> {
        let response = self
            .client
            .post(self.endpoint(SUBMIT_CONSTRAINTS_PATH))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&constraints)?)
            .send()
            .await?;

        if response.status() != StatusCode::OK {
            let error = response.json::<ErrorResponse>().await?;
            return Err(BuilderApiError::FailedSubmittingConstraints(error));
        }

        Ok(())
    }

    async fn get_header_with_proofs(
        &self,
        params: GetHeaderParams,
    ) -> Result<VersionedValue<SignedBuilderBid>, BuilderApiError> {
        let parent_hash = format!("0x{}", hex::encode(params.parent_hash.as_ref()));
        let public_key = format!("0x{}", hex::encode(params.public_key.as_ref()));

        let response = self
            .client
            .get(self.endpoint(&format!(
                "/eth/v1/builder/header_with_proofs/{}/{}/{}",
                params.slot, parent_hash, public_key,
            )))
            .header("content-type", "application/json")
            .send()
            .await?;

        if response.status() != StatusCode::OK {
            let error = response.json::<ErrorResponse>().await?;
            return Err(BuilderApiError::FailedGettingHeader(error));
        }

        let header = response.json::<VersionedValue<SignedBuilderBid>>().await?;

        if !matches!(header.version, Fork::Deneb) {
            return Err(BuilderApiError::InvalidFork(header.version.to_string()));
        };

        // TODO: verify proofs here?

        Ok(header)
    }

    async fn delegate(&self, signed_data: &[SignedDelegation]) -> Result<(), BuilderApiError> {
        let response = self
            .client
            .post(self.endpoint(DELEGATE_PATH))
            .header("content-type", "application/json")
            .body(serde_json::to_string(signed_data)?)
            .send()
            .await?;

        if response.status() != StatusCode::OK {
            let error = response.json::<ErrorResponse>().await?;
            return Err(BuilderApiError::FailedDelegating(error));
        }

        Ok(())
    }

    async fn revoke(&self, signed_data: &[SignedRevocation]) -> Result<(), BuilderApiError> {
        let response = self
            .client
            .post(self.endpoint(REVOKE_PATH))
            .header("content-type", "application/json")
            .body(serde_json::to_string(signed_data)?)
            .send()
            .await?;

        if response.status() != StatusCode::OK {
            let error = response.json::<ErrorResponse>().await?;
            return Err(BuilderApiError::FailedRevoking(error));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use reqwest::Url;

    use super::ConstraintsClient;

    #[test]
    fn test_join_endpoints() {
        let client = ConstraintsClient::new(Url::parse("http://localhost:8080/").unwrap());
        assert_eq!(
            client.endpoint("/eth/v1/builder/header/1/0x123/0x456"),
            Url::parse("http://localhost:8080/eth/v1/builder/header/1/0x123/0x456").unwrap()
        );

        assert_eq!(
            client.endpoint("eth/v1/builder/validators"),
            Url::parse("http://localhost:8080/eth/v1/builder/validators").unwrap()
        );
    }
}
