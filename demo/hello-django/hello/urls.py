from django.http import HttpResponse
from django.urls import path


def index(request):
    return HttpResponse("<h1>Hello, world!</h1>\n")


urlpatterns = [path("", index)]
