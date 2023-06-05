from pydantic import BaseModel

from app.services.task_creator import ObjectType


class CreateTaskData(BaseModel):
    object_id: int
    object_type: ObjectType
    file_format: str
    allowed_langs: list[str]
