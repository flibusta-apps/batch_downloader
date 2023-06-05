from pydantic import BaseSettings


class Config(BaseSettings):
    API_KEY: str

    REDIS_HOST: str
    REDIS_PORT: int
    REDIS_DB: int
    REDIS_PASSWORD: str | None

    MINIO_HOST: str
    MINIO_BUCKET: str
    MINIO_ACCESS_KEY: str
    MINIO_SECRET_KEY: str

    LIBRARY_API_KEY: str
    LIBRARY_URL: str

    CACHE_API_KEY: str
    CACHE_URL: str

    SENTRY_DSN: str | None


env_config = Config()

REDIS_URL = (
    f"redis://{env_config.REDIS_HOST}:{env_config.REDIS_PORT}/{env_config.REDIS_DB}"
)
