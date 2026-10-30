"""JSON Lines transport for an external Monika extension.

The handler receives (operation, input, context). During resolve it can call
context.read(resource) or context.read_many(resources) to obtain current
Observations selected by Core. Local Resources use project-relative paths.
An observe handler writes raw bytes to input["content_path"] and returns state.
"""

import json
import pathlib
import sys


class Context:
    def __init__(self, extension, request):
        self.workspace = pathlib.Path(request["workspace"])
        self._extension = extension
        self._request = request
        self._observations = {}

    def read(self, resource):
        return self.read_many([resource])[0]

    def read_many(self, resources):
        if self._request["operation"] != "resolve":
            raise ValueError("observations can only be requested during resolve")
        resources = list(resources)
        missing = list(dict.fromkeys(r for r in resources if r not in self._observations))
        if missing:
            ident = self._request["id"]
            self._extension.send({"protocol": 1, "id": ident, "read": missing})
            while True:
                response = self._extension.receive()
                if "operation" in response:
                    # The same process can be the resource provider. Serve its
                    # observe request while our interpretation is awaiting data.
                    if response["operation"] != "observe":
                        raise ValueError("expected a provider observation request")
                    self._extension.dispatch(response)
                    continue
                if response["id"] != ident:
                    raise ValueError("observation response ID mismatch")
                if "read_error" in response:
                    raise RuntimeError(response["read_error"])
                observations = response["read_result"]
                if len(observations) != len(missing):
                    raise ValueError("observation response length mismatch")
                self._observations.update(zip(missing, observations))
                break
        return [self._observations[r] for r in resources]


class Extension:
    def __init__(self, handler):
        self.handler = handler

    def send(self, message):
        print(json.dumps(message, ensure_ascii=False), flush=True)

    def receive(self):
        line = sys.stdin.readline()
        if not line:
            raise EOFError("Core closed stdin during an observation request")
        message = json.loads(line)
        if message["protocol"] != 1:
            raise ValueError("unsupported protocol")
        return message

    def dispatch(self, request):
        if request["protocol"] != 1:
            raise ValueError("unsupported protocol")
        context = Context(self, request)
        try:
            response = {"result": self.handler(request["operation"], request["input"], context)}
        except Exception as error:
            response = {"error": str(error)}
        self.send({"protocol": 1, "id": request["id"], **response})

    def run(self):
        for line in sys.stdin:
            self.dispatch(json.loads(line))
