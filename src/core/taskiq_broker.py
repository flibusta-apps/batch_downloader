from redis.asyncio import Redis
from taskiq import TaskiqEvents, TaskiqState
from taskiq_redis import ListQueueBroker, RedisAsyncResultBackend

from core.config import REDIS_URL


result_backend = RedisAsyncResultBackend(redis_url=REDIS_URL, result_ex_time=5 * 60)

broker = ListQueueBroker(url=REDIS_URL).with_result_backend(result_backend)


@broker.on_event(TaskiqEvents.WORKER_STARTUP)
async def startup(state: TaskiqState) -> None:
    state.redis = Redis.from_url(REDIS_URL)


@broker.on_event(TaskiqEvents.WORKER_SHUTDOWN)
async def shutdown(state: TaskiqState) -> None:
    await state.redis.close()
