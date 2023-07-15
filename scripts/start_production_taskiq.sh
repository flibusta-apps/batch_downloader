cd /app

taskiq worker core.taskiq_broker:broker app.services.downloader -w 1
