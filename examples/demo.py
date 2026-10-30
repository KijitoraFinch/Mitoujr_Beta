"""Run real jq pipelines with explicitly configured external test extensions."""

import argparse
import json
import os
import pathlib
import subprocess
import tempfile

REPOSITORY = pathlib.Path(__file__).resolve().parent.parent


def demonstrate(binary, directory):
    directory = directory.resolve()
    environment = {**os.environ, "M": str(binary)}

    def run(script, expected=0):
        process = subprocess.run(
            ["bash", "-o", "pipefail", "-c", script], cwd=directory,
            env=environment, text=True, capture_output=True, check=False,
        )
        if process.returncode != expected:
            raise RuntimeError(f"{script}: exit={process.returncode}\n{process.stdout}\n{process.stderr}")
        print(f"exit={expected}: {script}")
        return [json.loads(line) for line in process.stdout.splitlines()]

    run('"$M" init')
    extension = REPOSITORY / "tests/fixtures/sets_extension.py"
    (directory / "monika.json").write_text(json.dumps({
        "version": 1,
        "extensions": {"sets": {
            "command": ["python3", str(extension)], "revision": "test-v1", "schemes": ["file"],
        }},
        "names": {"all": {"extension": "sets", "resource": "rows.json", "query": "0"}},
        "checks": [{"extension": "sets"}],
    }, indent=2) + "\n")
    (directory / "rows.json").write_text("[1, 2, 3]\n")
    link = {
        "id": "design-to-data",
        "source": {"name": "all"},
        "target": {"extension": "sets", "resource": "rows.json", "query": "2"},
        "annotation": {"relation": "supports", "expected_count": 2},
    }
    (directory / "monika.links.json").write_text(json.dumps({"version": 1, "links": [link]}, indent=2) + "\n")
    run('"$M" extension list')
    run('"$M" extension show sets')
    values = run('"$M" links | jq -c \'select(.annotation.relation == "supports")\' | "$M" read --at /target')
    assert values[0]["region"]["content"]["count"] == 2
    values = run('"$M" links | jq -c \'{link:., boundary:{name:"all"}}\' | "$M" compare --left /link/target --right /boundary | jq -c \'select(.relation == "within" or .relation == "equal") | .input.link\' | "$M" check')
    assert values[0]["valid"]
    run('"$M" links | "$M" check --duplicates')
    baseline = run('"$M" capture --link design-to-data --endpoint target')[0]["captures"]["target"]
    run('"$M" links | "$M" read --at /target --captured')
    standalone = run('echo \'{"name":"all"}\' | "$M" capture')[0]["capture"]
    run(f'"$M" gc --dry-run --keep {standalone}')
    cases = [{"name":"all"}] + [{"extension": "sets", "resource": "rows.json", "query": str(n)} for n in [0, 2, 3, 4]]
    (directory / "cases.json").write_text(json.dumps(cases))
    assert run('"$M" extension test --cases cases.json')[0]["passed"]
    (directory / "rows.json").write_text("[1, 2, 3, 4]\n")
    run('"$M" links | "$M" check', expected=1)
    assert run('"$M" links | "$M" read --at /target')[0]["region"]["content"]["count"] == 3
    assert run('"$M" links | "$M" read --captured --at /target')[0]["region"]["content"]["count"] == 2
    # Adopt an expensive Resource explicitly, then keep it fixed across commands.
    config_path = directory / "monika.json"
    config = json.loads(config_path.read_text())
    config["resources"] = {"rows.json": "refreshed"}
    config["checks"] = []
    config_path.write_text(json.dumps(config))
    run('"$M" links | "$M" read --at /target', expected=2)
    run('"$M" refresh rows.json')
    run('"$M" capture --link design-to-data --endpoint both')
    (directory / "rows.json").write_text("[1, 2, 3, 4, 5]\n")
    run('"$M" links | "$M" check')
    run('"$M" refresh --all')
    run('"$M" links | "$M" check', expected=1)
    removed = run(f'"$M" gc --keep {standalone}')[0]
    assert baseline in removed["captures"]
    print(f"Verified workspace: {directory}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--monika", type=pathlib.Path, default=REPOSITORY / "target/debug/monika")
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
