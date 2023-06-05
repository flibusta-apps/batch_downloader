from fastapi import FastAPI
from fastapi.responses import ORJSONResponse

from redis.asyncio import Redis

from app.views import router
from core.config import REDIS_URL
from core.taskiq_broker import broker


def start_app() -> FastAPI:
    app = FastAPI(default_response_class=ORJSONResponse)

    redis = Redis.from_url(REDIS_URL)
    app.state.redis = redis

    app.include_router(router)

    @app.on_event("startup")
    async def app_startup():
        if not broker.is_worker_process:
            await broker.startup()

    @app.on_event("shutdown")
    async def app_shutdown():
        if not broker.is_worker_process:
            await broker.shutdown()

        await redis.close()

    return app
