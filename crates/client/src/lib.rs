//! A client library for Warg component registries.

#![deny(missing_docs)]

use crate::storage::PackageInfo;
use anyhow::{anyhow, Context, Result};
use reqwest::{Body, IntoUrl};
use std::cmp::Ordering;
use std::{borrow::Cow, collections::HashMap, path::PathBuf, time::Duration};
use storage::{
    ContentStorage, FileSystemContentStorage, FileSystemRegistryStorage, PublishInfo,
    RegistryStorage,
};
use thiserror::Error;
use warg_api::v1::{
    fetch::{FetchError, FetchLogsRequest, FetchLogsResponse},
    package::{
        MissingContent, PackageError, PackageRecord, PackageRecordState, PublishRecordRequest,
        UploadEndpoint,
    },
    proof::{ConsistencyRequest, InclusionRequest},
};
use warg_crypto::{
    hash::{AnyHash, Sha256},
    signing, Encode, Signable,
};
use warg_protocol::{
    operator, package,
    registry::{LogId, LogLeaf, PackageName, RecordId, RegistryLen, TimestampedCheckpoint},
    PublishedProtoEnvelope, SerdeEnvelope, Version, VersionReq,
};

pub mod api;
mod config;
pub mod lock;
mod registry_url;
pub mod storage;
pub use self::config::*;
pub use self::registry_url::RegistryUrl;

/// A client for a Warg registry.
pub struct Client<R, C> {
    registry: R,
    content: C,
    api: api::Client,
}

impl<R: RegistryStorage, C: ContentStorage> Client<R, C> {
    /// Creates a new client for the given URL, registry storage, and
    /// content storage.
    pub fn new(url: impl IntoUrl, registry: R, content: C) -> ClientResult<Self> {
        Ok(Self {
            registry,
            content,
            api: api::Client::new(url)?,
        })
    }

    /// Gets the URL of the client.
    pub fn url(&self) -> &RegistryUrl {
        self.api.url()
    }

    /// Gets the registry storage used by the client.
    pub fn registry(&self) -> &R {
        &self.registry
    }

    /// Gets the content storage used by the client.
    pub fn content(&self) -> &C {
        &self.content
    }

    /// Reset client storage for the registry.
    pub async fn reset_registry(&self, all_registries: bool) -> ClientResult<()> {
        tracing::info!("resetting registry local state");
        self.registry
            .reset(all_registries)
            .await
            .or(Err(ClientError::ResettingRegistryLocalStateFailed))
    }

    /// Clear client content cache.
    pub async fn clear_content_cache(&self) -> ClientResult<()> {
        tracing::info!("removing content cache");
        self.content
            .clear()
            .await
            .or(Err(ClientError::ClearContentCacheFailed))
    }

    /// Submits the publish information in client storage.
    ///
    /// If there's no publishing information in client storage, an error is returned.
    ///
    /// Returns the identifier of the record that was published.
    ///
    /// Use `wait_for_publish` to wait for the record to transition to the `published` state.
    pub async fn publish(&self, signing_key: &signing::PrivateKey) -> ClientResult<RecordId> {
        let info = self
            .registry
            .load_publish()
            .await?
            .ok_or(ClientError::NotPublishing)?;

        let res = self.publish_with_info(signing_key, info).await;
        self.registry.store_publish(None).await?;
        res
    }

    /// Submits the provided publish information.
    ///
    /// Any publish information in client storage is ignored.
    ///
    /// Returns the identifier of the record that was published.
    ///
    /// Use `wait_for_publish` to wait for the record to transition to the `published` state.
    pub async fn publish_with_info(
        &self,
        signing_key: &signing::PrivateKey,
        mut info: PublishInfo,
    ) -> ClientResult<RecordId> {
        if info.entries.is_empty() {
            return Err(ClientError::NothingToPublish {
                name: info.name.clone(),
            });
        }

        let initializing = info.initializing();

        tracing::info!(
            "publishing {new}package `{name}`",
            name = info.name,
            new = if initializing { "new " } else { "" }
        );
        tracing::debug!("entries: {:?}", info.entries);

        let mut package = self
            .registry
            .load_package(&info.name)
            .await?
            .unwrap_or_else(|| PackageInfo::new(info.name.clone()));

        // If we're not initializing the package and a head was not explicitly specified,
        // updated to the latest checkpoint to get the latest known head.
        if !initializing && info.head.is_none() {
            self.update_checkpoint(&self.api.latest_checkpoint().await?, [&mut package])
                .await?;

            info.head = package.state.head().as_ref().map(|h| h.digest.clone());
        }

        match (initializing, info.head.is_some()) {
            (true, true) => {
                return Err(ClientError::CannotInitializePackage { name: package.name })
            }
            (false, false) => {
                return Err(ClientError::MustInitializePackage { name: package.name })
            }
            _ => (),
        }

        let record = info.finalize(signing_key)?;
        let log_id = LogId::package_log::<Sha256>(&package.name);
        let record = self
            .api
            .publish_package_record(
                &log_id,
                PublishRecordRequest {
                    package_name: Cow::Borrowed(&package.name),
                    record: Cow::Owned(record.into()),
                    content_sources: Default::default(),
                },
            )
            .await
            .map_err(|e| {
                ClientError::translate_log_not_found(e, |id| {
                    if id == &log_id {
                        Some(package.name.clone())
                    } else {
                        None
                    }
                })
            })?;

        // TODO: parallelize this
        for (digest, MissingContent { upload }) in record.missing_content() {
            // Upload the missing content, if the registry supports it
            let Some(UploadEndpoint::Http {
                method,
                url,
                headers,
            }) = upload.first()
            else {
                continue;
            };

            self.api
                .upload_content(
                    method,
                    url,
                    headers,
                    Body::wrap_stream(self.content.load_content(digest).await?.ok_or_else(
                        || ClientError::ContentNotFound {
                            digest: digest.clone(),
                        },
                    )?),
                )
                .await
                .map_err(|e| match e {
                    api::ClientError::Package(PackageError::Rejection(reason)) => {
                        ClientError::PublishRejected {
                            name: package.name.clone(),
                            record_id: record.record_id.clone(),
                            reason,
                        }
                    }
                    _ => e.into(),
                })?;
        }

        Ok(record.record_id)
    }

    /// Waits for a package record to transition to the `published` state.
    ///
    /// The `interval` is the amount of time to wait between checks.
    ///
    /// Returns an error if the package record was rejected.
    pub async fn wait_for_publish(
        &self,
        package: &PackageName,
        record_id: &RecordId,
        interval: Duration,
    ) -> ClientResult<()> {
        let log_id = LogId::package_log::<Sha256>(package);
        let mut current = self.get_package_record(package, &log_id, record_id).await?;

        loop {
            match current.state {
                PackageRecordState::Sourcing { .. } => {
                    return Err(ClientError::PackageMissingContent);
                }
                PackageRecordState::Published { .. } => {
                    return Ok(());
                }
                PackageRecordState::Rejected { reason } => {
                    return Err(ClientError::PublishRejected {
                        name: package.clone(),
                        record_id: record_id.clone(),
                        reason,
                    });
                }
                PackageRecordState::Processing => {
                    tokio::time::sleep(interval).await;
                    current = self.get_package_record(package, &log_id, record_id).await?;
                }
            }
        }
    }

    /// Updates every package log in client storage to the latest registry checkpoint.
    pub async fn update(&self) -> ClientResult<()> {
        tracing::info!("updating all packages to latest checkpoint");

        let mut updating = self.registry.load_packages().await?;
        self.update_checkpoint(&self.api.latest_checkpoint().await?, &mut updating)
            .await?;

        Ok(())
    }

    /// Inserts or updates the logs of the specified packages in client storage to
    /// the latest registry checkpoint.
    pub async fn upsert<'a, I>(&self, packages: I) -> Result<(), ClientError>
    where
        I: IntoIterator<Item = &'a PackageName>,
        I::IntoIter: ExactSizeIterator,
    {
        tracing::info!("updating specific packages to latest checkpoint");

        let packages = packages.into_iter();
        let mut updating = Vec::with_capacity(packages.len());
        for package in packages {
            updating.push(
                self.registry
                    .load_package(package)
                    .await?
                    .unwrap_or_else(|| PackageInfo::new(package.clone())),
            );
        }

        self.update_checkpoint(&self.api.latest_checkpoint().await?, &mut updating)
            .await?;

        Ok(())
    }

    /// Downloads the latest version of a package into client storage that
    /// satisfies the given version requirement.
    ///
    /// If the requested package log is not present in client storage, it
    /// will be fetched from the registry first.
    ///
    /// An error is returned if the package does not exist.
    ///
    /// If a version satisfying the requirement does not exist, `None` is
    /// returned.
    ///
    /// Returns the path within client storage of the package contents for
    /// the resolved version.
    pub async fn download(
        &self,
        name: &PackageName,
        requirement: &VersionReq,
    ) -> Result<Option<PackageDownload>, ClientError> {
        tracing::info!("downloading package `{name}` with requirement `{requirement}`");
        let info = self.fetch_package(name).await?;

        match info.state.find_latest_release(requirement) {
            Some(release) => {
                let digest = release
                    .content()
                    .context("invalid state: not yanked but missing content")?
                    .clone();
                let path = self.download_content(&digest).await?;
                Ok(Some(PackageDownload {
                    version: release.version.clone(),
                    digest,
                    path,
                }))
            }
            None => Ok(None),
        }
    }

    /// Downloads the specified version of a package into client storage.
    ///
    /// If the requested package log is not present in client storage, it
    /// will be fetched from the registry first.
    ///
    /// An error is returned if the package does not exist.
    ///
    /// Returns the path within client storage of the package contents for
    /// the specified version.
    pub async fn download_exact(
        &self,
        package: &PackageName,
        version: &Version,
    ) -> Result<PackageDownload, ClientError> {
        tracing::info!("downloading version {version} of package `{package}`");
        let info = self.fetch_package(package).await?;

        let release =
            info.state
                .release(version)
                .ok_or_else(|| ClientError::PackageVersionDoesNotExist {
                    version: version.clone(),
                    name: package.clone(),
                })?;

        let digest = release
            .content()
            .ok_or_else(|| ClientError::PackageVersionDoesNotExist {
                version: version.clone(),
                name: package.clone(),
            })?;

        Ok(PackageDownload {
            version: version.clone(),
            digest: digest.clone(),
            path: self.download_content(digest).await?,
        })
    }

    async fn update_checkpoint<'a>(
        &self,
        ts_checkpoint: &SerdeEnvelope<TimestampedCheckpoint>,
        packages: impl IntoIterator<Item = &mut PackageInfo>,
    ) -> Result<(), ClientError> {
        let checkpoint = &ts_checkpoint.as_ref().checkpoint;
        tracing::info!(
            "updating to checkpoint log length `{}`",
            checkpoint.log_length
        );

        let mut operator = self.registry.load_operator().await?.unwrap_or_default();

        // Map package names to package logs that need to be updated
        let mut packages = packages
            .into_iter()
            .filter_map(|p| match &p.checkpoint {
                // Don't bother updating if the package is already at the specified checkpoint
                Some(c) if c == checkpoint => None,
                _ => Some((LogId::package_log::<Sha256>(&p.name), p)),
            })
            .inspect(|(_, p)| tracing::info!("package `{name}` will be updated", name = p.name))
            .collect::<HashMap<_, _>>();
        if packages.is_empty() {
            return Ok(());
        }

        let mut last_known = packages
            .iter()
            .map(|(id, p)| (id.clone(), p.head_fetch_token.clone()))
            .collect::<HashMap<_, _>>();

        loop {
            let response: FetchLogsResponse = self
                .api
                .fetch_logs(FetchLogsRequest {
                    log_length: checkpoint.log_length,
                    operator: operator
                        .head_fetch_token
                        .as_ref()
                        .map(|t| Cow::Borrowed(t.as_str())),
                    limit: None,
                    packages: Cow::Borrowed(&last_known),
                })
                .await
                .map_err(|e| {
                    ClientError::translate_log_not_found(e, |id| {
                        packages.get(id).map(|p| p.name.clone())
                    })
                })?;

            for record in response.operator {
                let proto_envelope: PublishedProtoEnvelope<operator::OperatorRecord> =
                    record.envelope.try_into()?;

                // skip over records that has already seen
                if operator.head_registry_index.is_none()
                    || proto_envelope.registry_index > operator.head_registry_index.unwrap()
                {
                    operator.state = operator
                        .state
                        .validate(&proto_envelope.envelope)
                        .map_err(|inner| ClientError::OperatorValidationFailed { inner })?;
                    operator.head_registry_index = Some(proto_envelope.registry_index);
                    operator.head_fetch_token = Some(record.fetch_token);
                }
            }

            for (log_id, records) in response.packages {
                let package = packages.get_mut(&log_id).ok_or_else(|| {
                    anyhow!("received records for unknown package log `{log_id}`")
                })?;

                for record in records {
                    let proto_envelope: PublishedProtoEnvelope<package::PackageRecord> =
                        record.envelope.try_into()?;

                    // skip over records that has already seen
                    if package.head_registry_index.is_none()
                        || proto_envelope.registry_index > package.head_registry_index.unwrap()
                    {
                        let state = std::mem::take(&mut package.state);
                        package.state =
                            state.validate(&proto_envelope.envelope).map_err(|inner| {
                                ClientError::PackageValidationFailed {
                                    name: package.name.clone(),
                                    inner,
                                }
                            })?;
                        package.head_registry_index = Some(proto_envelope.registry_index);
                        package.head_fetch_token = Some(record.fetch_token);
                    }
                }

                // At this point, the package log should not be empty
                if package.state.head().is_none() {
                    return Err(ClientError::PackageLogEmpty {
                        name: package.name.clone(),
                    });
                }
            }

            if !response.more {
                break;
            }

            // Update the last known record fetch token for each package log
            for (id, fetch_token) in last_known.iter_mut() {
                *fetch_token = packages[id].head_fetch_token.clone();
            }
        }

        // verify checkpoint signature
        TimestampedCheckpoint::verify(
            operator.state.public_key(ts_checkpoint.key_id()).ok_or(
                ClientError::InvalidCheckpointKeyId {
                    key_id: ts_checkpoint.key_id().clone(),
                },
            )?,
            &ts_checkpoint.as_ref().encode(),
            ts_checkpoint.signature(),
        )
        .or(Err(ClientError::InvalidCheckpointSignature))?;

        // Prove inclusion for the current log heads
        let mut leaf_indices = Vec::with_capacity(packages.len() + 1 /* for operator */);
        let mut leafs = Vec::with_capacity(leaf_indices.len());

        // operator record inclusion
        if let Some(index) = operator.head_registry_index {
            leaf_indices.push(index);
            leafs.push(LogLeaf {
                log_id: LogId::operator_log::<Sha256>(),
                record_id: operator.state.head().as_ref().unwrap().digest.clone(),
            });
        } else {
            return Err(ClientError::NoOperatorRecords);
        }

        // package records inclusion
        for (log_id, package) in &packages {
            if let Some(index) = package.head_registry_index {
                leaf_indices.push(index);
                leafs.push(LogLeaf {
                    log_id: log_id.clone(),
                    record_id: package.state.head().as_ref().unwrap().digest.clone(),
                });
            } else {
                return Err(ClientError::PackageLogEmpty {
                    name: package.name.clone(),
                });
            }
        }

        if !leafs.is_empty() {
            self.api
                .prove_inclusion(
                    InclusionRequest {
                        log_length: checkpoint.log_length,
                        leafs: leaf_indices,
                    },
                    checkpoint,
                    &leafs,
                )
                .await?;
        }

        if let Some(from) = self.registry.load_checkpoint().await? {
            let from_log_length = from.as_ref().checkpoint.log_length;
            let to_log_length = ts_checkpoint.as_ref().checkpoint.log_length;

            match from_log_length.cmp(&to_log_length) {
                Ordering::Greater => {
                    return Err(ClientError::CheckpointLogLengthRewind {
                        from: from_log_length,
                        to: to_log_length,
                    });
                }
                Ordering::Less => {
                    self.api
                        .prove_log_consistency(
                            ConsistencyRequest {
                                from: from_log_length,
                                to: to_log_length,
                            },
                            Cow::Borrowed(&from.as_ref().checkpoint.log_root),
                            Cow::Borrowed(&ts_checkpoint.as_ref().checkpoint.log_root),
                        )
                        .await?
                }
                Ordering::Equal => {
                    if from.as_ref().checkpoint.log_root
                        != ts_checkpoint.as_ref().checkpoint.log_root
                        || from.as_ref().checkpoint.map_root
                            != ts_checkpoint.as_ref().checkpoint.map_root
                    {
                        return Err(ClientError::CheckpointChangedLogRootOrMapRoot {
                            log_length: from_log_length,
                        });
                    }
                }
            }
        }

        self.registry.store_operator(operator).await?;

        for package in packages.values_mut() {
            package.checkpoint = Some(checkpoint.clone());
            self.registry.store_package(package).await?;
        }

        self.registry.store_checkpoint(ts_checkpoint).await?;

        Ok(())
    }

    async fn fetch_package(&self, name: &PackageName) -> Result<PackageInfo, ClientError> {
        match self.registry.load_package(name).await? {
            Some(info) => {
                tracing::info!("log for package `{name}` already exists in storage");
                Ok(info)
            }
            None => {
                let mut info = PackageInfo::new(name.clone());
                self.update_checkpoint(&self.api.latest_checkpoint().await?, [&mut info])
                    .await?;

                Ok(info)
            }
        }
    }

    async fn get_package_record(
        &self,
        package: &PackageName,
        log_id: &LogId,
        record_id: &RecordId,
    ) -> ClientResult<PackageRecord> {
        let record = self
            .api
            .get_package_record(log_id, record_id)
            .await
            .map_err(|e| {
                ClientError::translate_log_not_found(e, |id| {
                    if id == log_id {
                        Some(package.clone())
                    } else {
                        None
                    }
                })
            })?;
        Ok(record)
    }

    /// Downloads the content for the specified digest into client storage.
    ///
    /// If the content already exists in client storage, the existing path
    /// is returned.
    pub async fn download_content(&self, digest: &AnyHash) -> Result<PathBuf, ClientError> {
        match self.content.content_location(digest) {
            Some(path) => {
                tracing::info!("content for digest `{digest}` already exists in storage");
                Ok(path)
            }
            None => {
                self.content
                    .store_content(
                        Box::pin(self.api.download_content(digest).await?),
                        Some(digest),
                    )
                    .await?;

                self.content
                    .content_location(digest)
                    .ok_or_else(|| ClientError::ContentNotFound {
                        digest: digest.clone(),
                    })
            }
        }
    }
}

/// A Warg registry client that uses the local file system to store
/// package logs and content.
pub type FileSystemClient = Client<FileSystemRegistryStorage, FileSystemContentStorage>;

/// A result of an attempt to lock client storage.
pub enum StorageLockResult<T> {
    /// The storage lock was acquired.
    Acquired(T),
    /// The storage lock was not acquired for the specified directory.
    NotAcquired(PathBuf),
}

impl FileSystemClient {
    /// Attempts to create a client for the given registry URL.
    ///
    /// If the URL is `None`, the default URL is used; if there is no default
    /// URL, an error is returned.
    ///
    /// If a lock cannot be acquired for a storage directory, then
    /// `NewClientResult::Blocked` is returned with the path to the
    /// directory that could not be locked.
    pub fn try_new_with_config(
        url: Option<&str>,
        config: &Config,
    ) -> Result<StorageLockResult<Self>, ClientError> {
        let StoragePaths {
            registry_url: url,
            registries_dir,
            content_dir,
        } = config.storage_paths_for_url(url)?;

        let (packages, content) = match (
            FileSystemRegistryStorage::try_lock(registries_dir.clone())?,
            FileSystemContentStorage::try_lock(content_dir.clone())?,
        ) {
            (Some(packages), Some(content)) => (packages, content),
            (None, _) => return Ok(StorageLockResult::NotAcquired(registries_dir)),
            (_, None) => return Ok(StorageLockResult::NotAcquired(content_dir)),
        };

        Ok(StorageLockResult::Acquired(Self::new(
            url.into_url(),
            packages,
            content,
        )?))
    }

    /// Creates a client for the given registry URL.
    ///
    /// If the URL is `None`, the default URL is used; if there is no default
    /// URL, an error is returned.
    ///
    /// This method blocks if storage locks cannot be acquired.
    pub fn new_with_config(url: Option<&str>, config: &Config) -> Result<Self, ClientError> {
        let StoragePaths {
            registry_url,
            registries_dir,
            content_dir,
        } = config.storage_paths_for_url(url)?;
        Self::new(
            registry_url.into_url(),
            FileSystemRegistryStorage::lock(registries_dir)?,
            FileSystemContentStorage::lock(content_dir)?,
        )
    }
}

/// Represents information about a downloaded package.
#[derive(Debug, Clone)]
pub struct PackageDownload {
    /// The package version that was downloaded.
    pub version: Version,
    /// The digest of the package contents.
    pub digest: AnyHash,
    /// The path to the downloaded package contents.
    pub path: PathBuf,
}

/// Represents an error returned by Warg registry clients.
#[derive(Debug, Error)]
pub enum ClientError {
    /// No default registry server URL is configured.
    #[error("no default registry server URL is configured")]
    NoDefaultUrl,

    /// Reset registry local state.
    #[error("reset registry state failed")]
    ResettingRegistryLocalStateFailed,

    /// Clearing content local cache.
    #[error("clear content cache failed")]
    ClearContentCacheFailed,

    /// Checkpoint signature failed verification
    #[error("invalid checkpoint signature")]
    InvalidCheckpointSignature,

    /// Checkpoint signature failed verification
    #[error("invalid checkpoint key ID `{key_id}`")]
    InvalidCheckpointKeyId {
        /// The signature key ID.
        key_id: signing::KeyID,
    },

    /// The server did not provide operator records.
    #[error("the server did not provide any operator records")]
    NoOperatorRecords,

    /// The operator failed validation.
    #[error("operator failed validation: {inner}")]
    OperatorValidationFailed {
        /// The validation error.
        inner: operator::ValidationError,
    },

    /// The package already exists and cannot be initialized.
    #[error("package `{name}` already exists and cannot be initialized")]
    CannotInitializePackage {
        /// The package name that already exists.
        name: PackageName,
    },

    /// The package must be initialized before publishing.
    #[error("package `{name}` must be initialized before publishing")]
    MustInitializePackage {
        /// The name of the package that must be initialized.
        name: PackageName,
    },

    /// There is no publish operation in progress.
    #[error("there is no publish operation in progress")]
    NotPublishing,

    /// The package has no records to publish.
    #[error("package `{name}` has no records to publish")]
    NothingToPublish {
        /// The package that has no publish operations.
        name: PackageName,
    },

    /// The package does not exist.
    #[error("package `{name}` does not exist")]
    PackageDoesNotExist {
        /// The missing package.
        name: PackageName,
    },

    /// The package version does not exist.
    #[error("version `{version}` of package `{name}` does not exist")]
    PackageVersionDoesNotExist {
        /// The missing version of the package.
        version: Version,
        /// The package with the missing version.
        name: PackageName,
    },

    /// The package failed validation.
    #[error("package `{name}` failed validation: {inner}")]
    PackageValidationFailed {
        /// The package that failed validation.
        name: PackageName,
        /// The validation error.
        inner: package::ValidationError,
    },

    /// Content was not found during a publish operation.
    #[error("content with digest `{digest}` was not found in client storage")]
    ContentNotFound {
        /// The digest of the missing content.
        digest: AnyHash,
    },

    /// The package log is empty and cannot be validated.
    #[error("package log is empty and cannot be validated")]
    PackageLogEmpty {
        /// The package with an empty package log.
        name: PackageName,
    },

    /// A publish operation was rejected.
    #[error("the publishing of package `{name}` was rejected due to: {reason}")]
    PublishRejected {
        /// The package that was rejected.
        name: PackageName,
        /// The record identifier for the record that was rejected.
        record_id: RecordId,
        /// The reason it was rejected.
        reason: String,
    },

    /// The package is still missing content.
    #[error("the package is still missing content after all content was uploaded")]
    PackageMissingContent,

    /// The registry provided a latest checkpoint with a log length less than a previously provided
    /// checkpoint log length.
    #[error("registry rewinded checkpoints; latest checkpoint log length `{to}` is less than previously received checkpoint log length `{from}`")]
    CheckpointLogLengthRewind {
        /// The previously received checkpoint log length.
        from: RegistryLen,
        /// The latest checkpoint log length.
        to: RegistryLen,
    },

    /// The registry provided a checkpoint with a different `log_root` and
    /// `map_root` than a previously provided checkpoint.
    #[error("registry provided a new checkpoint with the same log length `{log_length}` as previously fetched but different log root or map root")]
    CheckpointChangedLogRootOrMapRoot {
        /// The checkpoint log length.
        log_length: RegistryLen,
    },

    /// An error occurred during an API operation.
    #[error(transparent)]
    Api(#[from] api::ClientError),

    /// An error occurred while performing a client operation.
    #[error("{0:?}")]
    Other(#[from] anyhow::Error),
}

impl ClientError {
    fn translate_log_not_found(
        e: api::ClientError,
        lookup: impl Fn(&LogId) -> Option<PackageName>,
    ) -> Self {
        match &e {
            api::ClientError::Fetch(FetchError::LogNotFound(id))
            | api::ClientError::Package(PackageError::LogNotFound(id)) => {
                if let Some(name) = lookup(id) {
                    return Self::PackageDoesNotExist { name };
                }
            }
            _ => {}
        }

        Self::Api(e)
    }
}

/// Represents the result of a client operation.
pub type ClientResult<T> = Result<T, ClientError>;
