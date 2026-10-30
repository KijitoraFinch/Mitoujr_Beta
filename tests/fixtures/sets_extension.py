"""A real process extension used to exercise multi-observation interpretation."""

import hashlib
import os
import json
import pathlib
import sys
from urllib.parse import unquote_to_bytes, urlparse


sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "examples"))
from extension_api import Extension


NAME = sys.argv[1] if len(sys.argv) > 1 else "sets"

def resource_path(resource):
    parsed = urlparse(resource)
    if parsed.scheme not in ("file", "fixture") or parsed.netloc not in ("", "localhost"):
        raise ValueError("expected a local file or fixture URI")
    if parsed.query or parsed.fragment:
        raise ValueError("resource URI cannot contain a query or fragment")
    path = pathlib.Path(os.fsdecode(unquote_to_bytes(parsed.path)))
    if not path.is_absolute():
        raise ValueError("resource URI must contain an absolute path")
    return path


def read_resource(resource):
    path = resource_path(resource)
    try:
        content = path.read_bytes()
        stat = path.stat()
    except FileNotFoundError:
        return b"", {"exists": False, "revision": {"parent": os.fsencode(path.parent.resolve()).hex()}}
    return content, {"exists": True, "revision": {
        "hash": hashlib.sha256(content).hexdigest(), "device": stat.st_dev,
        "inode": stat.st_ino, "mtime": stat.st_mtime_ns,
        "path": os.fsencode(path.resolve()).hex(),
    }}


def query_for(selector, workspace):
    query = selector["query"]
    if isinstance(query, str) or query is None:
        resource = selector["resource"]
        return {"resources": [resource], "minimum": int(query or 0)}
    return query


def execute(operation, data, context):
    workspace = context.workspace
    if operation == "describe":
        return {
            "name": NAME,
            "revision": "test-v1",
            "operations": ["observe", "resolve", "compare", "check"],
            "domains": ["test.sets.v1"],
            "description": (
                "Select numeric rows from JSON arrays. Selector: "
                '{"extension":"sets","resource":"rows.json","query":"2"} '
                "selects values >= 2. Alternatively query is "
                '{"resources":["file:///project/rows.json"],"minimum":2}. '
                'Content: {"count":number}. Annotation expected_count is checked against target count. '
            ),
        }
    if operation == "observe":
        content, state = read_resource(data["resource"])
        pathlib.Path(data["content_path"]).write_bytes(content)
        return {"state": state}
    if operation == "resolve":
        query = query_for(data["selector"], workspace)
        selected = []
        for item in context.read_many(query["resources"]):
            observation = item["observation"]
            if not observation["state"]["exists"]:
                raise ValueError("resource does not exist: " + observation["resource"])
            rows = json.loads(pathlib.Path(item["content_path"]).read_bytes())
            selected.extend(
                [observation["id"], index]
                for index, row in enumerate(rows)
                if row >= query.get("minimum", 0)
            )
        return {
            "domain": "test.sets.v1",
            "identity": sorted(selected),
            "content": {"count": len(selected)},
        }
    if operation == "compare":
        if NAME == "broken":
            return "contains"
        left = {tuple(item) for item in data["left"]["identity"]}
        right = {tuple(item) for item in data["right"]["identity"]}
        if left == right:
            return "equal"
        if left > right:
            return "contains"
        if left < right:
            return "within"
        return "incomparable"
    if operation == "check":
        annotation = data["link"]["annotation"]
        if not isinstance(annotation, dict):
            return [{"status": "not_applicable", "code": "no_expectation", "message": "No count expectation"}]
        expected = annotation.get("expected_count")
        if expected is None:
            return [{"status": "not_applicable", "code": "no_expectation", "message": "No count expectation"}]
        actual = data["target"]["region"]["content"]["count"]
        return [{"status": "pass" if actual == expected else "fail", "code": "count", "message": f"Expected {expected}, observed {actual}"}]
    raise ValueError("unsupported operation: " + operation)


if __name__ == "__main__":
    Extension(execute).run()
