"""Exercise the CLI with explicitly configured external test extensions."""

import argparse
import json
import pathlib
import subprocess
import tempfile


REPOSITORY = pathlib.Path(__file__).resolve().parent.parent


def demonstrate(binary, directory):
    directory = directory.resolve()
    extension = REPOSITORY / "tests" / "fixtures" / "sets_extension.py"
    config = {
        "version": 1,
        "extensions": {
            "sets": {"command": ["python3", str(extension)], "revision": "test-v1", "schemes": ["file"]},
            "lines": {"command": ["python3", str(extension), "lines"], "revision": "test-v1"},
        },
        "comparators": {"test.sets.v1": "sets"},
        "checks": [{"extension": "sets"}],
        "names": {"subset": {"extension": "sets", "resource": "rows.json", "query": "2"}},
    }

    def run(*arguments, value=None, expected=0):
        process = subprocess.run(
            [str(binary), "--root", str(directory), "--json", *arguments],
            input=None if value is None else json.dumps(value),
            text=True, capture_output=True, check=False,
        )
        if process.returncode != expected:
            raise RuntimeError(f"{arguments}: exit={process.returncode}\n{process.stdout}\n{process.stderr}")
        result = json.loads(process.stdout)
        print(f"exit={expected}: {' '.join(arguments)}")
        return result

    run("init")
    (directory / "monika.json").write_text(json.dumps(config, indent=2) + "\n")
    (directory / "rows.json").write_text("[1, 2, 3]\n")
    link = {
        "id": "design-to-data",
        "source": {"extension": "sets", "resource": "rows.json", "query": "0"},
        "reference": {"target": {"name": "subset"}},
        "annotation": {"relation": "supports", "expected_count": 2},
    }
    run("extension", "list")
    run("extension", "show", "sets")
    shown = run("show", "@subset")
    assert shown["data"]["region"]["content"]["count"] == 2
    run("link", "add", "--link", "-", value=link)
    links = run("links")
    assert links["data"]["links"][0]["annotation"] == link["annotation"]
    run("links", "--where", 'annotation.relation == "supports" and target within region("@subset")')
    run("links", "--to", "@subset", "--within")
    run("check")
    run("request", value={"op": "links", "where": {
        "op": "compare", "field": "annotation.relation", "relation": "equal",
        "value": {"kind": "json", "value": "supports"},
    }})
    link["annotation"]["review"] = {"owner": "利用者", "notes": [None, "reviewed"]}
    prepared = run("link", "set", "--link", "-", "--patch-out", "review.patch.json", value=link)
    assert prepared["data"]["applied"] is False
    run("apply", str(directory / "review.patch.json"))
    current = run("links")["data"]["links"][0]
    run("generate", "output.jsonl", "--extension", "lines")
    assert json.loads((directory / "output.jsonl").read_text()) == current
    run("link", "remove", "design-to-data")
    run("import", "output.jsonl", "--extension", "lines")
    imported = run("links")
    assert imported["data"]["links"][0] == current
    assert len(imported["data"]["origins"]) == 1
    before_repeat = (directory / "monika.links.json").read_bytes()
    run("import", "output.jsonl", "--extension", "lines")
    assert (directory / "monika.links.json").read_bytes() == before_repeat
    cases = [{"extension": "sets", "resource": "rows.json", "query": str(minimum)} for minimum in [0, 2, 3, 4]]
    tested = run("extension", "test", "--cases", "-", value=cases)
    assert tested["data"]["passed"]
    (directory / "rows.json").write_text("[1, 2, 3, 4]\n")
    run("check", expected=1)
    latest = run("show", "@subset")
    assert latest["data"]["region"]["content"]["count"] == 3
    print(f"Verified workspace: {directory}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--monika", type=pathlib.Path, default=REPOSITORY / "target" / "debug" / "monika")
    parser.add_argument("--directory", type=pathlib.Path, help="Create and keep this new directory")
    args = parser.parse_args()
    binary = args.monika.resolve(strict=True)
    if args.directory:
        args.directory.mkdir(parents=True, exist_ok=False)
        demonstrate(binary, args.directory)
    else:
        with tempfile.TemporaryDirectory(prefix="monika-demo-") as directory:
            demonstrate(binary, pathlib.Path(directory))


if __name__ == "__main__":
    main()
