//! This module tries to abstract the persistent storage backend. The
//! abstraction is not perfect as S3 leaks through pretty heavily. :)

use std::{
    collections::BTreeMap,
    {path::Path, time::Duration},
};

use anyhow::{anyhow, Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use crate::error::RequestError;

/// The persistent configuration that lives in the S3 bucket as
/// /channels.json.
#[derive(Deserialize, Debug, Clone)]
struct PersistentChannelsConfig {
    /// The list of all channels we serve. Each channel needs a
    /// corresponding <channel>.json file for configuration in the
    /// bucket.
    channels: Vec<String>,
}

/// The persistent configuration of a single channel.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChannelConfig {
    /// The latest element in the channel. If this is foo, users can download it as channel/foo.tar.gz.
    pub latest: String,

    /// Previous tarballs in this channel.
    #[serde(default)]
    pub previous: Vec<String>,
}

/// The list of channels we know about and their latest object keys.
#[derive(Debug, Default, Clone)]
pub struct ChannelsConfig {
    /// A mapping from channel name to latest object key.
    channels: BTreeMap<String, ChannelConfig>,
}

impl ChannelsConfig {
    pub fn channels(&self) -> impl Iterator<Item = &str> {
        self.channels.keys().map(|s| s.as_ref())
    }

    pub fn channel(&self, channel_name: &str) -> Option<ChannelConfig> {
        self.channels.get(channel_name).cloned()
    }
}

pub struct Client {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl Client {
    /// Open an S3 client with configuration from the environment.
    // TODO Return a custom error type.
    pub async fn new_from_env(bucket: &str) -> Result<Client> {
        let amzn_config = aws_config::load_from_env().await;
        let s3_config = aws_sdk_s3::config::Builder::from(&amzn_config)
            // TODO For minio compat. Should this be configurable?
            .force_path_style(true)
            .build();

        Ok(Self {
            client: aws_sdk_s3::Client::from_conf(s3_config),
            bucket: bucket.to_owned(),
        })
    }

    /// Read a file from S3 into memory. This should only be used for
    /// small files.
    // TODO Return a custom error type.
    async fn read_file(&self, object_key: &str) -> Result<Bytes> {
        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(object_key)
            .send()
            .await
            // TODO Better error.
            .with_context(|| format!("Failed to read: {object_key}"))?;

        Ok(response.body.collect().await?.into_bytes())
    }

    // TODO Return a custom error type.
    pub async fn load_channels_config(&self) -> Result<ChannelsConfig> {
        let persistent_config: PersistentChannelsConfig =
            serde_json::from_slice(&self.read_file("channels.json").await?)
                .context("Failed to deserialize channels.json")?;

        debug!("Loaded channel config: {persistent_config:?}");

        let mut channels_config = ChannelsConfig::default();

        for channel_name in persistent_config.channels {
            let config_file = format!("{channel_name}.json");
            if let Ok(channel_config) = self
                .read_file(&config_file)
                .await
                .context("Failed to read channel config")
                .and_then(|bytes| {
                    serde_json::from_slice::<ChannelConfig>(&bytes)
                        .context("Failed to deserialize channel configuration")
                })
            {
                info!(
                    "Channel {channel_name} points to: {}",
                    channel_config.latest
                );
                channels_config
                    .channels
                    .insert(channel_name, channel_config);
            } else {
                error!("Configured channel {channel_name:?} has no corresponding {config_file} in the bucket. Ignoring!");
                continue;
            }
        }

        Ok(channels_config)
    }

    /// Return a signed request for a specific object key in the bucket.
    pub async fn sign_request(&self, object_key: &str) -> Result<String, RequestError> {
        use aws_sdk_s3::presigning::PresigningConfig;

        Ok(self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(object_key)
            // TODO Should expiration be configurable?
            .presigned(
                PresigningConfig::expires_in(Duration::from_secs(600))
                    .map_err(|_e| RequestError::PresignConfigFailure)?,
            )
            .await
            .map_err(|_e| RequestError::PresignFailure {
                object_key: object_key.to_owned(),
            })?
            .uri()
            .to_string())
    }

    /// Upload a file to the persistent store. Doesn't update any channel.
    async fn write_file(&self, object_key: &str, file: &Path) -> Result<()> {
        // We would want to stream the file and not load it all in
        // memory, but it results in XAmzContentSHA256Mismatch. :(
        let data = tokio::fs::read(file)
            .await
            .context("Failed to read input file")?;

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key)
            .body(data.into())
            .send()
            .await
            .with_context(|| format!("Failed to upload file: {}", file.display()))?;

        Ok(())
    }

    async fn write_data(&self, object_key: &str, data: Vec<u8>) -> Result<()> {
        let data = ByteStream::from(data.to_owned());

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key)
            .body(data)
            .send()
            .await
            .context("Failed to upload file")?;

        Ok(())
    }

    /// Update the channel to point to the given file.
    ///
    /// **Note:** This operation is not concurrency-safe! Clients must
    /// serialize update operations.
    pub async fn update_channel(&self, channel_name: &str, file: &Path) -> Result<()> {
        // Path::ends_with and Path::extension unfortunately don't do
        // what we need.
        if !file
            .as_os_str()
            .to_str()
            .ok_or_else(|| anyhow!("File name is not valid UTF-8"))?
            .ends_with(".tar.xz")
        {
            return Err(anyhow!(
                "Invalid file ending. Only .tar.xz is supported: {}",
                file.display()
            ));
        }

        let channels_config = self.load_channels_config().await?;
        let mut channel = channels_config
            .channel(channel_name)
            .ok_or_else(|| anyhow!("Channel {channel_name} does not exit!"))?;

        let object_key = file
            .file_name()
            .ok_or_else(|| anyhow!("No file name: {}", file.display()))?
            .to_str()
            .ok_or_else(|| anyhow!("File name needs to be valid UTF-8: {}", file.display()))?
            .to_owned();

        let basename = object_key
            .strip_suffix(".tar.xz")
            // This unwrap is safe, because we checked the suffix earlier.
            .unwrap()
            .to_owned();

        self.write_file(&object_key, file).await?;

        println!(
            "Updating channel {channel_name} from {} to {}.",
            channel.latest, object_key
        );

        channel.previous.push(channel.latest);
        channel.latest = basename;

        self.write_data(
            &format!("{channel_name}.json"),
            serde_json::to_vec_pretty(&channel).context("Failed to serialize channel")?,
        )
        .await.context("Failed to update channel. This leaked the tarball! Remove it manually, if this is an issue.")?;

        Ok(())
    }
}
