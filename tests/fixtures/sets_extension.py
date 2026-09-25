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


def query_for(selector):
    query = selector["query"]
    if isinstance(query, str) or query is None:
        resource = selector["resource"]
        if not urlparse(resource).scheme:
            resource = (pathlib.Path(WORKSPACE) / resource).resolve().as_uri()
        return {"resources": [resource], "minimum": int(query or 0)}
    return query


def execute(operation, data):
    if operation == "describe":
        return {
            "name": NAME,
            "revision": "test-v1",
            "operations": ["observe", "dependencies", "resolve", "compare", "extract", "check_plan", "check", "edit", "commit"],
            "domains": ["test.sets.v1"],
            "description": "Select rows from multiple observed JSON resources",
        }
    if operation == "observe":
        content, state = read_resource(data["resource"])
        return {"bytes": list(content), "state": state}
    if operation == "dependencies":
        return query_for(data["selector"])["resources"]
    if operation == "resolve":
        query = query_for(data["selector"])
        selected, observations = [], []
        for item in data["observations"]:
            observation = item["observation"]
            if observation["resource"] not in query["resources"]:
                continue
            observations.append(observation["id"])
            rows = json.loads(pathlib.Path(item["content_path"]).read_bytes())
            selected.extend(
                [observation["id"], index]
                for index, row in enumerate(rows)
                if row >= query.get("minimum", 0)
            )
        return {
            "domain": "test.sets.v1",
            "identity": sorted(selected),
            "equivalence_key": hashlib.sha256(json.dumps(sorted(selected), separators=(",", ":")).encode()).hexdigest(),
            "observations": sorted(observations),
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
    if operation == "extract":
        observation = data["observation"]
        declarations = decode(pathlib.Path(data["content_path"]).read_bytes())["links"]
        return {
            "occurrences": [
                {
                    "declaration": declaration,
                    "evidence": {
                        "resource": observation["resource"],
                        "observation": observation["id"],
                        "extension": NAME,
                        "location": {"index": index},
                        "writable": True,
                    },
                }
                for index, declaration in enumerate(declarations)
            ],
            "diagnostics": [],
        }
    if operation == "edit":
        content = pathlib.Path(data["content_path"]).read_bytes()
        document = decode(content) if data["observation"]["state"]["exists"] else {"links": []}
        change = data["change"]
        links = document["links"]
        location = None
        if change["op"] == "add":
            location = {"index": len(links)}
            links.append(change["declaration"])
        elif change["op"] == "set":
            location = change["location"]
            links[location["index"]] = change["declaration"]
        elif change["op"] == "remove":
            del links[change["location"]["index"]]
        else:
            raise ValueError("unsupported edit")
        if NAME == "tamper" and location is not None:
            document["links"][location["index"]]["annotation"] = {"changed": True}
        return {"bytes": list(encode(document)), "location": location}
    if operation == "commit":
        if NAME == "no_write":
            _, state = read_resource(data["resource"])
            return {"content_hash": data["replacement"]["content_hash"], "state": state}
        path = resource_path(data["resource"])
        lock_directory = pathlib.Path(WORKSPACE) / ".monika" / "fixture-locks"
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
                marker = pathlib.Path(WORKSPACE) / "commit-completed.json"
                commits = json.loads(marker.read_text())["commits"] if marker.exists() else 0
                staged_marker = marker.with_suffix(".tmp")
                staged_marker.write_text(json.dumps({"commits": commits + 1, "pid": os.getpid()}))
                staged_marker.replace(marker)
                time.sleep(60)
            return {"content_hash": hashlib.sha256(content).hexdigest(), "state": state}
    if operation == "check_plan":
        baseline = data["link"]["occurrences"][0]["declaration"]["annotation"]
        if isinstance(baseline, dict) and "baseline" in baseline:
            return [{"name": "baseline", **baseline["baseline"]}]
        return []
    if operation == "check":
        annotation = data["link"]["occurrences"][0]["declaration"]["annotation"]
        if not isinstance(annotation, dict):
            return [{"status": "not_applicable", "code": "no_expectation", "message": "No count expectation"}]
        expected = annotation.get("expected_count")
        if "baseline" in data["regions"]:
            expected = data["regions"]["baseline"]["content"]["count"]
        if expected is None:
            return [{"status": "not_applicable", "code": "no_expectation", "message": "No count expectation"}]
        actual = data["link"]["target"]["region"]["content"]["count"]
        return [{"status": "pass" if actual == expected else "fail", "code": "count", "message": f"Expected {expected}, observed {actual}"}]
    raise ValueError("unsupported operation: " + operation)


for line in sys.stdin:
    request = json.loads(line)
    WORKSPACE = request["workspace"]
    if request["protocol"] != 1:
        raise ValueError("unsupported protocol")
    try:
        response = {"result": execute(request["operation"], request["input"])}
    except Exception as error:
        response = {"error": str(error)}
    print(json.dumps({"protocol": 1, "id": request["id"], **response}), flush=True)
