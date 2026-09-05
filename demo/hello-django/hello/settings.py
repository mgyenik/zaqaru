"""The smallest Django that serves a page.

Deliberately minimal: no database, no sessions, no static files. What the
demo exercises is the *stack* — nginx in front, gunicorn's prefork worker
behind, Django rendering — and every setting that would add a subsystem
would add syscalls that are not what the trace is measuring.
"""

SECRET_KEY = "not-a-secret-this-container-serves-one-page"
DEBUG = False
# The request arrives through nginx with whatever `Host` curl sent.
ALLOWED_HOSTS = ["*"]
ROOT_URLCONF = "hello.urls"
INSTALLED_APPS: list[str] = []
MIDDLEWARE: list[str] = []
TEMPLATES: list[dict] = []
DATABASES: dict = {}
USE_TZ = True
