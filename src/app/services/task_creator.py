import uuid

from pydantic import BaseModel
from redis.asyncio import Redis

from app.services.downloader import download
from app.services.library_client import LibraryClient, SequenceBook
from app.services.task_manager import ObjectType, Task, TaskManager


class CreateTaskError(BaseModel):
    message: str


class TaskCreator:
    @classmethod
    async def _get_books(
        cls, object_id: int, object_type: ObjectType, allowed_langs: list[str]
    ) -> list[SequenceBook] | CreateTaskError:
        books = []

        current_page = 1
        pages_count = 1

        match object_type:
            case ObjectType.SEQUENCE:
                books_getter = LibraryClient.get_sequence_books
            case ObjectType.AUTHOR:
                books_getter = LibraryClient.get_author_books
            case ObjectType.TRANSLATOR:
                books_getter = LibraryClient.get_translator_books

        while current_page <= pages_count:
            book_page = await books_getter(object_id, allowed_langs, page=current_page)
            if book_page is None:
                return CreateTaskError(message="Can't get books!")

            books.extend(book_page.items)

            current_page += 1
            pages_count = book_page.pages

        if len(books) == 0:
            return CreateTaskError(message="No books!")

        return books

    @classmethod
    async def _create_subtasks(
        cls,
        task_id: uuid.UUID,
        object_id: int,
        object_type: ObjectType,
        file_format: str,
        allowed_langs: list[str],
    ) -> list[str] | CreateTaskError:
        books = await cls._get_books(object_id, object_type, allowed_langs)
        if isinstance(books, CreateTaskError):
            return books

        task_ids: list[str] = []

        for book in books:
            if file_format not in book.available_types:
                continue

            task = await download.kiq(str(task_id), book.id, file_format)
            task_ids.append(task.task_id)

        if len(task_ids) == 0:
            return CreateTaskError(message="No books to archive!")

        return task_ids

    @classmethod
    async def create_task(
        cls,
        redis: Redis,
        object_id: int,
        object_type: ObjectType,
        file_format: str,
        allowed_langs: list[str],
    ) -> Task | CreateTaskError:
        task_id = uuid.uuid4()

        subtasks = await cls._create_subtasks(
            task_id, object_id, object_type, file_format, allowed_langs
        )
        if isinstance(subtasks, CreateTaskError):
            return subtasks

        task = Task(
            id=task_id, object_id=object_id, object_type=object_type, subtasks=subtasks
        )

        is_saved = await TaskManager.save_task(redis, task)
        if not is_saved:
            return CreateTaskError(message="Save task error")

        return task
