import enum
import uuid

from pydantic import BaseModel
from redis.asyncio import Redis, RedisError


class TaskStatusEnum(enum.StrEnum):
    IN_PROGRESS = "in_progress"
    ARCHIVING = "archiving"
    COMPLETE = "complete"


class ObjectType(enum.StrEnum):
    SEQUENCE = "sequence"
    AUTHOR = "author"


class Task(BaseModel):
    id: uuid.UUID
    object_id: int
    object_type: ObjectType
    subtasks: list[str]
    status: TaskStatusEnum = TaskStatusEnum.IN_PROGRESS
    result_link: str | None = None


class TaskManager:
    @classmethod
    def _get_key(cls, task_id: uuid.UUID) -> str:
        return f"at_{task_id}"

    @classmethod
    async def save_task(cls, redis: Redis, task: Task) -> bool:
        key = cls._get_key(task.id)

        try:
            data = task.json()
            await redis.set(key, data, ex=60 * 60)

            return True
        except RedisError:
            return False

    @classmethod
    async def get_task(cls, redis: Redis, task_id: uuid.UUID) -> Task | None:
        key = cls._get_key(task_id)

        try:
            data = await redis.get(key)
            if data is None:
                return None

            return Task.parse_raw(data)
        except RedisError:
            return None
