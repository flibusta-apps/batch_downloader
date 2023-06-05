from typing import Generic, TypeVar

import httpx
from pydantic import BaseModel
from pydantic.generics import GenericModel

from core.config import env_config


class SequenceBook(BaseModel):
    id: int
    available_types: list[str]


class AuthorBook(BaseModel):
    id: int
    available_types: list[str]


Item = TypeVar("Item", bound=BaseModel)


class Page(GenericModel, Generic[Item]):
    items: list[Item]
    total: int
    page: int
    size: int
    total_pages: int


class Sequence(BaseModel):
    id: int
    name: str


class Author(BaseModel):
    id: int
    first_name: str
    last_name: str
    middle_name: str | None = None


class LibraryClient:
    @staticmethod
    async def get_sequence_books(
        sequence_id: int, allowed_langs: list[str], page: int = 1
    ) -> Page[SequenceBook] | None:
        async with httpx.AsyncClient() as client:
            response = await client.get(
                f"{env_config.LIBRARY_URL}/api/v1/sequences/{sequence_id}/books",
                params={"page": page, "allowed_langs": allowed_langs},
                headers={"Authorization": env_config.LIBRARY_API_KEY},
            )

            if response.status_code != 200:
                return None

            return Page[SequenceBook].parse_raw(response.text)

    @staticmethod
    async def get_author_books(
        author_id: int, allowed_langs: list[str], page: int = 1
    ) -> Page[AuthorBook] | None:
        async with httpx.AsyncClient() as client:
            response = await client.get(
                f"{env_config.LIBRARY_URL}/api/v1/authors/{author_id}/books",
                params={"page": page, "allowed_langs": allowed_langs},
                headers={"Authorization": env_config.LIBRARY_API_KEY},
            )

            if response.status_code != 200:
                return None

            return Page[AuthorBook].parse_raw(response.text)

    @staticmethod
    async def get_sequence(sequence_id: int) -> Sequence | None:
        async with httpx.AsyncClient() as client:
            response = await client.get(
                f"{env_config.LIBRARY_URL}/api/v1/sequences/{sequence_id}",
                headers={"Authorization": env_config.LIBRARY_API_KEY},
            )

        if response.status_code != 200:
            return None

        return Sequence.parse_raw(response.text)

    @staticmethod
    async def get_author(author_id: int) -> Author | None:
        async with httpx.AsyncClient() as client:
            response = await client.get(
                f"{env_config.LIBRARY_URL}/api/v1/authors/{author_id}",
                headers={"Authorization": env_config.LIBRARY_API_KEY},
            )

        if response.status_code != 200:
            return None

        return Author.parse_raw(response.text)
