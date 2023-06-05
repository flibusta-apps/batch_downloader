import taskiq_fastapi
from taskiq_redis import ListQueueBroker, RedisAsyncResultBackend

from core.config import REDIS_URL


result_backend = RedisAsyncResultBackend(redis_url=REDIS_URL, result_ex_time=5 * 60)

broker = ListQueueBroker(url=REDIS_URL).with_result_backend(result_backend)

taskiq_fastapi.init(broker, "main:app")
