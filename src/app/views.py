from typing import Annotated
import uuid

from fastapi import APIRouter, Depends, HTTPException, status

from redis.asyncio import Redis

from app.depends import check_token, get_redis
from app.serializers import CreateTaskData
from app.services.task_creator import CreateTaskError, TaskCreator
from app.services.task_manager import TaskManager


router = APIRouter(prefix="/api", dependencies=[Depends(check_token)])


@router.post("/")
async def create_archive_task(
    redis: Annotated[Redis, Depends(get_redis)], data: CreateTaskData
):
    task = await TaskCreator.create_task(
        redis=redis,
        object_id=data.object_id,
        object_type=data.object_type,
        file_format=data.file_format,
        allowed_langs=data.allowed_langs,
    )

    if isinstance(task, CreateTaskError):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, task)

    return task


@router.get("/check_archive/{task_id}")
async def check_archive_task_status(
    redis: Annotated[Redis, Depends(get_redis)], task_id: uuid.UUID
):
    task = await TaskManager.get_task(redis, task_id)

    if task is None:
        raise HTTPException(status.HTTP_404_NOT_FOUND)

    return task
