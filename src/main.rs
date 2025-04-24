use anyhow::{Context, Result};
use clap::Parser;
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

mod cache;

#[derive(Parser)]
struct Args {
    /// The path to the config file.
    ///
    /// By default, this is a `jarss.toml` file in your config directory.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// The path to the cache directory.
    ///
    /// By default, this is `jarss` in your cache directory.
    #[arg(long)]
    cache: Option<PathBuf>,
    /// The path to the template to use in generating the feed.
    ///
    /// This should be a [`tera`] tempalte which takes a list of articles at `articles`, and
    /// generates a full HTML document.
    ///
    /// By default, this will use a simple HTML template, stored at `default-render.html.tera` in
    /// the repo. You can use this template as an example in writing your own.
    #[arg(long)]
    feed_template: Option<PathBuf>,
    /// The path the write the produced HTML page.
    out_html: PathBuf,
}

/// [`Args`] but with default values applied.
struct InferredArgs {
    /// The path to the config file.
    config: PathBuf,
    /// The path to the cache directory.
    cache: PathBuf,
    /// The template to use in generating the feed.
    feed_template: Box<str>,
    /// The path the write the produced HTML page.
    out_html: PathBuf,
}
impl TryFrom<Args> for InferredArgs {
    type Error = anyhow::Error;

    fn try_from(raw_args: Args) -> Result<Self> {
        let config = match raw_args.config {
            Some(config) => config,
            None => dirs::config_dir()
                .context("No default config directory on your system")?
                .join("jarss.toml"),
        };
        let cache = match raw_args.cache {
            Some(cache) => cache,
            None => dirs::cache_dir()
                .context("No default cache dir on your system")?
                .join("jarss"),
        };
        let feed_template = raw_args
            .feed_template
            .map_or_else(
                || Ok(include_str!("../default-render.html.tera").to_owned()),
                std::fs::read_to_string,
            )
            .context("Error reading feed template from file")?
            .into_boxed_str();
        Ok(InferredArgs {
            config,
            cache,
            feed_template,
            out_html: raw_args.out_html,
        })
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args: InferredArgs = Args::parse().try_into()?;
    let config = load_config(&args.config).with_context(|| {
        format!(
            "Couldn't load configuraion file at {}",
            args.config.display()
        )
    })?;
    let mut caches = cache::CacheManager::new(args.cache);

    // Fetch the feeds to check for updates
    let http_agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .timeout_per_call(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(10)))
            .build(),
    );
    for site in &config.sites {
        let cache = caches
            .get_mut(site)
            .with_context(|| format!("Error reading cache for {}", site.name))?;
        cache::query_site(&http_agent, &config, site, cache).context("Error fetching feed")?;
    }
    caches.save().context("Error saving caches")?;

    // Parse the articles and grab the most recent ones
    let mut articles = Vec::new();
    for (site_name, feed) in caches.feeds() {
        let mut feed = match feed {
            Ok(feed) => feed,
            Err(e) => {
                log::error!(
                    "{:?}",
                    e.context(format!("Error reading feed from {site_name}"))
                );
                continue;
            }
        };
        feed.entries
            .sort_unstable_by_key(|entry| std::cmp::Reverse(entry.published.or(entry.updated)));
        let feed_title = feed.title.as_mut().map_or(site_name, |title| {
            title.sanitize();
            &title.content
        });
        let newest_entries = match feed
            .entries
            .iter()
            .take(config.max_entries_per_site.unwrap_or(usize::MAX))
            .map(|entry| FeedEntryInfo::new(feed_title, entry))
            .collect::<Result<Vec<FeedEntryInfo>>>()
        {
            Ok(entries) => entries,
            Err(e) => {
                log::error!(
                    "{:?}",
                    e.context(format!("Error parsing entries in field from {site_name}"))
                );
                continue;
            }
        };
        articles.extend_from_slice(&newest_entries);
    }
    articles.sort_unstable_by_key(|article| std::cmp::Reverse(article.published));

    // Generate HTML output
    let mut tera = tera::Tera::default();
    tera.add_raw_template("output", &args.feed_template)
        .context("Error parsing tera template")?;
    let mut tera_ctx = tera::Context::new();
    tera_ctx.insert("articles", &articles);
    tera.render_to(
        "output",
        &tera_ctx,
        File::create(&args.out_html).context("Failed to open output file")?,
    )
    .context("Failed to write to output file")?;

    Ok(())
}

#[derive(Clone, Debug, serde::Serialize)]
struct FeedEntryInfo {
    site: Box<str>,
    published: chrono::DateTime<chrono::Utc>,
    publish_date: chrono::NaiveDate,
    title: Box<str>,
    link: Box<str>,
}
impl FeedEntryInfo {
    fn new(site_name: &str, entry: &feed_rs::model::Entry) -> Result<Self> {
        let published = entry
            .published
            .or(entry.updated)
            .context("Entry missing published time")?;
        Ok(Self {
            site: site_name.to_owned().into_boxed_str(),
            published,
            publish_date: published.date_naive(),
            title: {
                let mut title = entry.title.clone().context("Entry missing title")?;
                title.sanitize();
                title.content.into_boxed_str()
            },
            link: entry
                .links
                .first()
                .context("Entry missing link")?
                .href
                .clone()
                .into_boxed_str(),
        })
    }
}

/// The configuration file schema.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Config {
    /// The list of sites being used.
    sites: Vec<SiteConfig>,
    /// The minimum interval between fetches of the same site, in seconds.
    min_fetch_interval: u64,
    /// The maximum amount of entries from a given site.
    max_entries_per_site: Option<usize>,
    /// The maximum total amount of entries to display.
    max_total_entries: Option<usize>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct SiteConfig {
    /// The name of the site.
    name: Box<str>,
    /// The URL of the feed to read.
    feed_url: Box<str>,
}

/// Load the config from the given path.
fn load_config(path: impl AsRef<Path>) -> Result<Config> {
    let contents = std::fs::read_to_string(path).context("Failed to read config file")?;
    toml::de::from_str(&contents).context("Failed to parse config file")
}

const USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("GIT_DESCRIBE"),
    ") <",
    env!("CARGO_PKG_REPOSITORY"),
    "> RSS Feed Reader"
);
