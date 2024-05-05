use once_cell::sync::Lazy;

fn get_env(env: &'static str) -> String {
    std::env::var(env).unwrap_or_else(|_| panic!("Cannot get the {} env variable", env))
}

pub struct Config {
    pub api_key: String,

    pub library_api_key: String,
    pub library_url: String,

    pub cache_api_key: String,
    pub cache_url: String,

    pub sentry_dsn: String,
}

impl Config {
    pub fn load() -> Config {
        Config {
            api_key: get_env("API_KEY"),

            library_api_key: get_env("LIBRARY_API_KEY"),
            library_url: get_env("LIBRARY_URL"),

            cache_api_key: get_env("CACHE_API_KEY"),
            cache_url: get_env("CACHE_URL"),

            sentry_dsn: get_env("SENTRY_DSN"),
        }
    }
}

pub static CONFIG: Lazy<Config> = Lazy::new(Config::load);
