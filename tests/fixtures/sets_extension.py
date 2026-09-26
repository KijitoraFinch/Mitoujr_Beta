"""A real process extension used to exercise multi-observation interpretation."""

import fcntl
import hashlib
import os
import tempfile
import json
import pathlib
import sys
import time
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


def decode(content):
    if NAME == "lines":
        return {"links": [json.loads(line) for line in content.splitlines() if line.strip()]}
    return json.loads(content)


def encode(document):
    if NAME == "lines":
        return ("".join(json.dumps(link, ensure_ascii=False) + "\n" for link in document["links"])).encode()
    return (json.dumps(document, ensure_ascii=False, indent=2) + "\n").encode()


def query_for(selector, workspace):
    query = selector["query"]
    if isinstance(query, str) or query is None:
        resource = selector["resource"]
        if not urlparse(resource).scheme:
            resource = (workspace / resource).resolve().as_uri()
        return {"resources": [resource], "minimum": int(query or 0)}
    return query


def execute(operation, data, context):
    workspace = context.workspace
    if operation == "describe":
        return {
            "name": NAME,
            "revision": "test-v1",
            "operations": ["observe", "resolve", "compare", "import", "check_plan", "check", "generate", "commit"],
            "domains": ["test.sets.v1"],
            "description": "Select rows from multiple observed JSON resources",
        }
    if operation == "observe":
        content, state = read_resource(data["resource"])
        return {"bytes": list(content), "state": state}
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
    if operation == "import":
        links = decode(pathlib.Path(data["content_path"]).read_bytes())["links"]
        return {"links": [{"link": link, "location": {"index": index}}
                          for index, link in enumerate(links)],
                "diagnostics": ([{"stage": "import", "subject": "fixture", "message": "incomplete input"}]
                                if NAME == "partial" else [])}
    if operation == "generate":
        document = {"links": data["links"]}
        if NAME == "tamper" and document["links"]:
            document["links"][0]["annotation"] = {"changed": True}
        return {"bytes": list(encode(document))}
    if operation == "commit":
        if NAME == "no_write":
            _, state = read_resource(data["resource"])
            return {"content_hash": data["replacement"]["content_hash"], "state": state}
        path = resource_path(data["resource"])
        lock_directory = workspace / ".monika" / "fixture-locks"
        lock_directory.mkdir(parents=True, exist_ok=True)
        lock_path = lock_directory / hashlib.sha256(data["resource"].encode()).hexdigest()
        with lock_path.open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            content, state = read_resource(data["resource"])
            if state != data["expected"]["state"] or hashlib.sha256(content).hexdigest() != data["expected"]["content_hash"]:
                raise ValueError("resource changed at commit")
            if path.is_symlink():
                raise ValueError("refuse to replace a symlink")
            replacement = bytes(data["replacement"]["bytes"])
            if hashlib.sha256(replacement).hexdigest() != data["replacement"]["content_hash"]:
                raise ValueError("invalid replacement digest")
            with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as staged:
                staged.write(replacement)
                staged.flush()
                os.fsync(staged.fileno())
                if path.exists():
                    os.fchmod(staged.fileno(), path.stat().st_mode & 0o777)
            os.replace(staged.name, path)
            content, state = read_resource(data["resource"])
            if NAME == "pause_after_commit":
                marker = workspace / "commit-completed.json"
                commits = json.loads(marker.read_text())["commits"] if marker.exists() else 0
                staged_marker = marker.with_suffix(".tmp")
                staged_marker.write_text(json.dumps({"commits": commits + 1, "pid": os.getpid()}))
                staged_marker.replace(marker)
                time.sleep(60)
            return {"content_hash": hashlib.sha256(content).hexdigest(), "state": state}
    if operation == "check_plan":
        baseline = data["link"]["annotation"]
        if isinstance(baseline, dict) and "baseline" in baseline:
            return [{"name": "baseline", **baseline["baseline"]}]
        return []
    if operation == "check":
        annotation = data["link"]["annotation"]
        if not isinstance(annotation, dict):
            return [{"status": "not_applicable", "code": "no_expectation", "message": "No count expectation"}]
        expected = annotation.get("expected_count")
        if "baseline" in data["regions"]:
            expected = data["regions"]["baseline"]["content"]["count"]
        if expected is None:
            return [{"status": "not_applicable", "code": "no_expectation", "message": "No count expectation"}]
        actual = data["target"]["region"]["content"]["count"]
        return [{"status": "pass" if actual == expected else "fail", "code": "count", "message": f"Expected {expected}, observed {actual}"}]
    raise ValueError("unsupported operation: " + operation)


if __name__ == "__main__":
    Extension(execute).run()
