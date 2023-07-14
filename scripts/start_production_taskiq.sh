cd /app

taskiq worker core.taskiq_broker:broker -fsd -tp app/services/downloader.py
