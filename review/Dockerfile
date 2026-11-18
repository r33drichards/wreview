FROM python:3.13-slim
WORKDIR /app
COPY review/run.py review/worker.py review/prompt.md ./
CMD ["python", "-u", "worker.py"]
