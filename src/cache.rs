use super::{Config, SiteConfig};

use anyhow::{Context, Result};
use futures::Stream;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::{fs::File, sync::Mutex};

pub async fn query_site(
    agent: &reqwest::Client,
    config: &Config,
    site: &SiteConfig,
    cache: &mut SiteCache,
) -> Result<()> {
    let now = SystemTime::now();
    // Check if we've recently fetched, so we don't spam.
    if cache
        .last_fetch_time
        .is_some_and(|time| time + Duration::from_secs(config.min_fetch_interval) > now)
    {
        log::info!(
            "Site {} has been fetched recently, will not be fetched again",
            site.name
        );
        return Ok(());
    }
    // Check if we've been asked to retry later.
    if cache
        .last_retry_after
        .is_some_and(|retry_after| retry_after >= now)
    {
        log::warn!(
            "Site {} has 429 `retry-after`ed us, will not fetch for {}s",
            site.name,
            cache
                .last_retry_after
                .unwrap()
                .duration_since(now)
                .unwrap()
                .as_secs(),
        );
        return Ok(());
    }
    log::info!("Querying {}", site.name);
    let mut req = agent.get(site.feed_url.as_ref());
    if let Some(last_headers) = cache.last_headers.as_ref() {
        if let Some(etag) = last_headers.get("etag") {
            log::debug!("Found Etag {etag}");
            req = req.header("if-none-match", etag.as_ref());
        } else if let Some(last_modified) = last_headers.get("last-modified") {
            log::debug!("Found Last-Modified {last_modified}");
            req = req.header("if-modified-since", last_modified.as_ref());
        } else {
            log::warn!(
                "Uncached request sent to {} (only ok if this is our first request)",
                site.name
            );
        }
    }
    log::debug!("Sending request {req:?}");
    let res = req.send().await.context("Error fetching feed")?;
    match res.status() {
        http::status::StatusCode::OK => {
            log::info!("New content from {}", site.name);
            cache.last_headers = Some(
                res.headers()
                    .into_iter()
                    .map(|(key, value)| {
                        Ok::<_, anyhow::Error>((
                            key.as_str().to_owned().into_boxed_str(),
                            value
                                .to_str()
                                .with_context(|| format!("Invalid header {value:?}"))?
                                .to_owned()
                                .into_boxed_str(),
                        ))
                    })
                    .collect::<Result<HashMap<_, _>, _>>()
                    .context("Error parsing HTTP headers")?,
            );
            cache.last_body = Some(
                res.text()
                    .await
                    .context("Failed to read feed contents")?
                    .into_boxed_str(),
            );
            cache.last_fetch_time = Some(SystemTime::now());
            cache.last_retry_after = None;
            Ok(())
        }
        http::status::StatusCode::NOT_MODIFIED => {
            log::debug!("No new content from {}", site.name);
            cache.last_fetch_time = Some(SystemTime::now());
            Ok(())
        }
        http::status::StatusCode::TOO_MANY_REQUESTS => {
            log::warn!("Received 429 Too Many Requests from {}", site.name);
            // We were told to wait before the next request
            if let Some(retry_after) = res.headers().get("retry-after") {
                let Ok(Ok(interval)) = retry_after.to_str().map(str::parse::<u64>) else {
                    log::warn!("Malformed `retry-after` header: {retry_after:?}");
                    return Ok(());
                };
                cache.last_retry_after = Some(SystemTime::now() + Duration::from_secs(interval));
                Ok(())
            } else {
                log::error!("429 without `retry-after` header from {}", site.name);
                Ok(())
            }
        }
        status if !status.is_client_error() && !status.is_server_error() => {
            anyhow::bail!("Received unexpected status code {status}")
        }
        status => anyhow::bail!("Received error status code {status}"),
    }
}

pub struct CacheManager {
    cache_dir: PathBuf,
    caches: papaya::HashMap<Box<str>, Mutex<SiteCache>>,
}
impl CacheManager {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            caches: papaya::HashMap::new(),
        }
    }

    /// Return a guard for some operations that require it.
    pub fn cache_guard(&self) -> papaya::LocalGuard<'_> {
        self.caches.guard()
    }

    pub async fn get_mut<'a>(
        &self,
        index: &SiteConfig,
        guard: &'a papaya::LocalGuard<'a>,
    ) -> Result<impl std::ops::DerefMut<Target = SiteCache> + use<'_, 'a>> {
        if let Some(entry) = self.caches.get(&index.name, guard) {
            Ok(entry.lock().await)
        } else {
            let cache = SiteCache::load_for_site(&self.cache_dir, index).await?;
            let entry = self
                .caches
                .try_insert(index.name.clone(), Mutex::new(cache), guard)
                .unwrap();
            Ok(entry.lock().await)
        }
    }

    pub fn feeds<'a>(
        &self,
        guard: &'a papaya::LocalGuard<'a>,
    ) -> impl Stream<Item = (&'a str, Result<feed_rs::model::Feed>)> + use<'_, 'a> {
        use futures::StreamExt as _;
        futures::stream::iter(self.caches.iter(guard)).filter_map(async move |(site, cache)| {
            Some((
                site.as_ref(),
                feed_rs::parser::parse(std::io::Cursor::new(
                    cache.lock().await.last_body.as_ref()?.as_bytes(),
                ))
                .map_err(anyhow::Error::from),
            ))
        })
    }

    pub async fn save(&self) -> Result<()> {
        use futures::StreamExt as _;
        let caches = self.caches.pin();
        let mut saves = futures::stream::FuturesUnordered::new();
        for (site, cache) in caches.iter() {
            saves.push(async move {
                cache
                    .lock()
                    .await
                    .save_for_site(&self.cache_dir, site)
                    .await
                    .with_context(|| format!("Failed to save cache for {}", site))
            });
        }
        while let Some(res) = saves.next().await {
            res?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SiteCache {
    /// When the last `retry-after` said to retry, if we've been 429'ed.
    pub last_retry_after: Option<SystemTime>,
    /// The headers from the most recent successful fetch.
    pub last_headers: Option<HashMap<Box<str>, Box<str>>>,
    /// The body of the most recent successful fetch.
    pub last_body: Option<Box<str>>,
    /// The timestamp of the most recent successful fetch.
    pub last_fetch_time: Option<SystemTime>,
}
impl SiteCache {
    /// Load the cache entry for the given site.
    async fn load_for_site(cache_dir: impl AsRef<Path>, config: &SiteConfig) -> Result<Self> {
        let path = cache_dir
            .as_ref()
            .join(Self::cache_file_for_name(&config.name));
        match File::open(&path).await {
            Ok(mut file) => {
                use tokio::io::AsyncReadExt as _;
                let postcard_encoded = {
                    use std::io::Read;
                    let mut compressed = Vec::new();
                    file.read_to_end(&mut compressed).await?;
                    let mut encoded = Vec::new();
                    lz4_flex::frame::FrameDecoder::new(std::io::Cursor::new(compressed))
                        .read_to_end(&mut encoded)
                        .context("Failed to read cache file")?;
                    encoded
                };
                let res = postcard::from_bytes(&postcard_encoded)
                    .context("Failed to decode cache file")?;
                Ok(res)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("Generating empty cache for new site {}", config.name);
                Ok(Self::default())
            }
            Err(e) => Err(anyhow::Error::new(e).context("Failed to read cache entry")),
        }
    }

    /// Save the cache entry for the given site.
    async fn save_for_site(&self, cache_dir: impl AsRef<Path>, site_name: &str) -> Result<()> {
        use std::io::Write as _;
        use tokio::io::AsyncWriteExt as _;

        let _ = std::fs::create_dir_all(&cache_dir);
        let path = cache_dir
            .as_ref()
            .join(Self::cache_file_for_name(site_name));
        let encoded = postcard::to_stdvec(self).context("Error writing out cache")?;
        let compressed = {
            let mut lz4 = lz4_flex::frame::FrameEncoder::new(Vec::new());
            lz4.write_all(&encoded)?;
            lz4.finish()?
        };
        File::create(&path)
            .await
            .context("Error opening cache dir for writing")?
            .write_all(&compressed)
            .await
            .context("Error writing out cache")?;
        Ok(())
    }

    /// Turn a feed name into the name of the cache file.
    ///
    /// The name will be composed entirely of lower-case letters, numbers, and `-`s. Any characters
    /// which are not one of those, as well as any characters which lack a unique lower-case
    /// mapping, are excluded.
    ///
    /// Yes, this is slightly anglophone-centric, but this is an internal detail users shouldn't
    /// see, so I don't really care.
    fn cache_file_for_name(name: &str) -> String {
        let mut filename = name
            .chars()
            .filter_map(|c| {
                if c.is_alphanumeric() {
                    let mut lower_iter = c.to_lowercase();
                    let lower = lower_iter.next()?;
                    if lower_iter.next().is_some() {
                        return None;
                    }
                    Some(lower)
                } else if c.is_whitespace() || c == '-' || c == '_' {
                    Some('-')
                } else {
                    None
                }
            })
            .collect::<String>();
        filename += ".lz4";
        filename
    }
}
