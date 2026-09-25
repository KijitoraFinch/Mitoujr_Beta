"""Exercise observation reads through the same protocol used by real extensions."""

import json
import os
import pathlib
import sys
import time

import sets_extension as sets
from extension_api import Extension


calls = {}


def execute(operation, data, context):
    if operation == "observe" and sets.NAME == "provider":
        if data["resource"].endswith("unavailable.json"):
            raise RuntimeError("provider unavailable")
        if data["resource"].endswith("waiting.json"):
            marker = context.workspace / "provider-waiting.tmp"
            marker.write_text(str(os.getpid()))
            marker.replace(context.workspace / "provider-waiting")
            time.sleep(60)
    if (operation != "resolve" or not isinstance(data["selector"]["query"], dict)
            or "mode" not in data["selector"]["query"]):
        return sets.execute(operation, data, context)

    query = data["selector"]["query"]
    mode = query["mode"]
    if mode == "ignore_failure":
        try:
            context.read(query["resource"])
        except RuntimeError:
            pass
        return {"domain": "test.sets.v1", "identity": [], "content": {"count": 0}}
    if mode == "changing_reads":
        key = json.dumps(query, sort_keys=True)
        calls[key] = calls.get(key, 0) + 1
        if calls[key] > 1:
            context.read(query["extra"])
    if mode == "endless_reads":
        index = 0
        while True:
            context.read((context.workspace / f"absent-{index}.json").as_uri())
            index += 1
            time.sleep(0.02)

    minimum = 0
    if "optional" in query:
        config = context.read(query["optional"])
        if config["observation"]["state"]["exists"]:
            minimum = json.loads(pathlib.Path(config["content_path"]).read_bytes())["minimum"]
    item = context.read(query["resource"])
    while True:
        document = json.loads(pathlib.Path(item["content_path"]).read_bytes())
        if isinstance(document, dict) and "next" in document:
            item = context.read(document["next"])
        else:
            break
    # Repeated access in one interpretation uses its context, without another
    # round trip or a live read by the provider.
    context.read(query["resource"])
    identity = [[item["observation"]["id"], i] for i, row in enumerate(document) if row >= minimum]
    result = {"domain": "test.sets.v1", "identity": identity, "content": {"count": len(identity)}}
    if mode == "forge_dependencies":
        result["observations"] = ["not-a-real-observation"]
    return result


class TracedExtension(Extension):
    def dispatch(self, request):
        self.workspace = pathlib.Path(request["workspace"])
        self.record("input", request)
        super().dispatch(request)

    def record(self, direction, message):
        with (self.workspace / "wire.jsonl").open("a") as log:
            log.write(json.dumps({"pid": os.getpid(), "extension": sets.NAME,
                                  "direction": direction, "message": message}) + "\n")

    def receive(self):
        message = super().receive()
        if "operation" not in message:
            self.record("input", message)
        return message

    def send(self, message):
        self.record("output", message)
        super().send(message)


TracedExtension(execute).run()
