use std::{path::PathBuf, str::FromStr};

use once_cell::sync::Lazy;

fn get_env(env: &'static str) -> String {
    std::env::var(env).unwrap_or_else(|_| panic!("Cannot get the {} env variable", env))
}

/// Reads a required secret; refuses empty / whitespace-only values.
fn get_secret(env: &'static str) -> String {
    let value = get_env(env);
    if value.trim().is_empty() {
        panic!("The {env} env variable must not be empty");
    }
    value
}

/// Reads an optional numeric variable. Unset -> `default`; set but
/// unparsable or smaller than `min` -> startup failure (no silent default).
fn get_number<T>(env: &'static str, default: T, min: T) -> T
where
    T: FromStr + PartialOrd + std::fmt::Display + Copy,
    T::Err: std::fmt::Display,
{
    match std::env::var(env) {
        Err(_) => default,
        Ok(raw) => {
            let value = raw
                .trim()
                .parse::<T>()
                .unwrap_or_else(|err| panic!("Invalid value {raw:?} for {env}: {err}"));
            if value < min {
                panic!("Invalid value {raw:?} for {env}: must be >= {min}");
            }
            value
        }
    }
}

pub struct Config {
    pub api_key: String,

    pub library_api_key: String,
    pub library_url: String,

    pub cache_api_key: String,
    pub cache_url: String,

    /// Optional: Sentry is disabled when unset/empty.
    pub sentry_dsn: Option<String>,

    /// Maximum number of archive tasks running at the same time.
    pub max_concurrent_tasks: usize,
    /// Maximum number of tasks waiting for a free slot before new requests
    /// are rejected with 503.
    pub max_queued_tasks: usize,

    /// Dedicated directory for archive files.
    pub data_dir: PathBuf,
}

impl Config {
    pub fn load() -> Config {
        Config {
            api_key: get_secret("API_KEY"),

            library_api_key: get_secret("LIBRARY_API_KEY"),
            library_url: get_env("LIBRARY_URL"),

            cache_api_key: get_secret("CACHE_API_KEY"),
            cache_url: get_env("CACHE_URL"),

            sentry_dsn: std::env::var("SENTRY_DSN")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),

            max_concurrent_tasks: get_number("MAX_CONCURRENT_TASKS", 4usize, 1),
            max_queued_tasks: get_number("MAX_QUEUED_TASKS", 32usize, 0),

            data_dir: std::env::var("DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/tmp/batch_downloader")),
        }
    }
}

pub static CONFIG: Lazy<Config> = Lazy::new(Config::load);

/// Path of the finished archive for a task token.
pub fn archive_path(token: &str) -> PathBuf {
    CONFIG.data_dir.join(token)
}

/// Path of the archive while it is still being written.
pub fn partial_archive_path(token: &str) -> PathBuf {
    CONFIG.data_dir.join(format!("{token}.part"))
}

#[cfg(test)]
pub mod test_support {
    use std::sync::Once;

    /// Sets harmless fake env values (and a private temp data dir) before the
    /// first access to `CONFIG`.
    pub fn ensure_test_env() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let dir = tempfile::tempdir().unwrap().keep();
            std::env::set_var("API_KEY", "test-api-key");
            std::env::set_var("LIBRARY_API_KEY", "test-library-key");
            std::env::set_var("LIBRARY_URL", "http://localhost:0");
            std::env::set_var("CACHE_API_KEY", "test-cache-key");
            std::env::set_var("CACHE_URL", "http://localhost:0");
            std::env::set_var("DATA_DIR", dir);
            std::env::remove_var("SENTRY_DSN");
        });
    }
}
