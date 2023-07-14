import asyncio
from base64 import b64decode
from io import BytesIO
import tempfile
from typing import cast
import uuid
import zipfile

import httpx
from minio import Minio
from redis.asyncio import Redis
from taskiq import TaskiqDepends
from taskiq.task import AsyncTaskiqTask
from transliterate import translit

from app.depends import get_redis_taskiq
from app.services.library_client import LibraryClient
from app.services.task_manager import ObjectType, TaskManager, TaskStatusEnum
from core.config import env_config
from core.taskiq_broker import broker, result_backend


def get_minio_client():
    return Minio(
        env_config.MINIO_HOST,
        access_key=env_config.MINIO_ACCESS_KEY,
        secret_key=env_config.MINIO_SECRET_KEY,
        secure=False,
    )


async def _download_to_tmpfile(
    book_id: int, file_type: str, output: tempfile.SpooledTemporaryFile
) -> tuple[str, int] | None:
    async with httpx.AsyncClient() as client:
        request = client.build_request(
            "get",
            f"{env_config.CACHE_URL}/api/v1/download/{book_id}/{file_type}",
            headers={"Authorization": env_config.CACHE_API_KEY},
        )

        response = await client.send(request, stream=True)

        if response.status_code != 200:
            await response.aclose()
            return None

        filename = b64decode(response.headers["X-Filename-B64"]).decode()

        loop = asyncio.get_running_loop()

        async for chunk in response.aiter_bytes(2048):
            await loop.run_in_executor(None, output.write, chunk)

        await loop.run_in_executor(None, output.flush)
        await loop.run_in_executor(None, output.seek, 0, 2)
        size = await loop.run_in_executor(None, output.tell)
        await loop.run_in_executor(None, output.seek, 0)

    return filename, size


async def download_file_to_file(link: str, output: BytesIO) -> bool:
    async with httpx.AsyncClient() as client:
        request = client.build_request(
            "get", link, headers={"Authorization": env_config.CACHE_API_KEY}
        )

        response = await client.send(request, stream=True)

        if response.status_code != 200:
            await response.aclose()
            return False

        loop = asyncio.get_running_loop()

        async for chunk in response.aiter_bytes(2048):
            await loop.run_in_executor(None, output.write, chunk)

    return True


@broker.task()
async def download(task_id: str, book_id: int, file_type: str) -> str | None:
    try:
        with tempfile.SpooledTemporaryFile() as temp_file:
            data = await _download_to_tmpfile(book_id, file_type, temp_file)

            if data is None:
                return None

            filename, size = data

            minio_client = get_minio_client()

            loop = asyncio.get_event_loop()
            await loop.run_in_executor(
                None,
                minio_client.put_object,
                env_config.MINIO_BUCKET,
                filename,
                temp_file,
                size,
            )

            return filename
    finally:
        await check_subtasks.kiq(task_id)


async def _check_subtasks(subtasks: list[str]) -> bool:
    """
    Return `true` if all substask `.is_ready()`
    """

    internal_subtasks = [
        AsyncTaskiqTask(subtask, result_backend) for subtask in subtasks
    ]

    for task in internal_subtasks:
        task_is_ready = await task.is_ready()

        if not task_is_ready:
            return False

    return True


@broker.task()
async def check_subtasks(task_id: str, redis: Redis = TaskiqDepends(get_redis_taskiq)):
    task = await TaskManager.get_task(redis, uuid.UUID(task_id))

    if task is None:
        return False

    await asyncio.sleep(1)

    is_subtasks_ready = await _check_subtasks(task.subtasks)
    if is_subtasks_ready:
        await create_archive.kiq(task_id)


@broker.task()
async def create_archive(task_id: str, redis: Redis = TaskiqDepends(get_redis_taskiq)):
    task = await TaskManager.get_task(redis, uuid.UUID(task_id))
    assert task

    match task.object_type:
        case ObjectType.SEQUENCE:
            item = await LibraryClient.get_sequence(task.object_id)
            assert item
            name = item.name
        case ObjectType.AUTHOR | ObjectType.TRANSLATOR:
            item = await LibraryClient.get_author(task.object_id)
            assert item
            names = [item.first_name, item.last_name, item.middle_name]
            name = "_".join([i for i in names if i])

    # TODO: test with `uk` and `be`
    tr_name = translit(name, "ru", reversed=True, strict=True)

    archive_filename = f"{item.id}_{tr_name}.zip"

    assert item

    task.status = TaskStatusEnum.ARCHIVING
    await TaskManager.save_task(redis, task)

    minio_client = get_minio_client()

    loop = asyncio.get_running_loop()

    with tempfile.SpooledTemporaryFile() as temp_zipfile:
        zip_file = zipfile.ZipFile(
            temp_zipfile,
            mode="w",
            compression=zipfile.ZIP_DEFLATED,
            allowZip64=False,
            compresslevel=9,
        )

        for subtask_id in task.subtasks:
            subtask = AsyncTaskiqTask(subtask_id, result_backend)

            result = await subtask.get_result()

            if result.is_err:
                continue

            filename: str | None = result.return_value

            if filename is None:
                continue

            book_file_link = await loop.run_in_executor(
                None,
                minio_client.get_presigned_url,
                "GET",
                env_config.MINIO_BUCKET,
                filename,
            )

            with zip_file.open(filename, "w") as internal_zip_file:
                await download_file_to_file(
                    book_file_link, cast(BytesIO, internal_zip_file)
                )

            await loop.run_in_executor(
                None, minio_client.remove_object, env_config.MINIO_BUCKET, filename
            )

        zip_file.close()

        await loop.run_in_executor(None, temp_zipfile.flush)
        await loop.run_in_executor(None, temp_zipfile.seek, 0, 2)
        size = await loop.run_in_executor(None, temp_zipfile.tell)
        await loop.run_in_executor(None, temp_zipfile.seek, 0)

        await loop.run_in_executor(
            None,
            minio_client.put_object,
            env_config.MINIO_BUCKET,
            archive_filename,
            temp_zipfile,
            size,
        )

    task.status = TaskStatusEnum.COMPLETE
    task.result_filename = archive_filename
    task.result_link = await loop.run_in_executor(
        None,
        minio_client.get_presigned_url,
        "GET",
        env_config.MINIO_BUCKET,
        archive_filename,
    )
    await TaskManager.save_task(redis, task)
