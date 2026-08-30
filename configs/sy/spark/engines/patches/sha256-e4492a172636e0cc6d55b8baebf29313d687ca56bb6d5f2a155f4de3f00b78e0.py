#!/usr/bin/env python3
"""Qualify imports and the declarative launch command without model weights."""

import argparse
import importlib
import json
import os
import sys
import tomllib

from sglang.srt.server_args import ServerArgs
from sglang.srt.utils import is_sm120_supported


PLACEHOLDERS = {
    "{model_snapshot}": "/models/synthetic",
    "{served_model}": "synthetic",
    "{port}": "30000",
}


def expand(arguments):
    return [PLACEHOLDERS.get(argument, argument) for argument in arguments]


def main(profile_path, cuda_architecture):
    with open(profile_path, "rb") as handle:
        policy = tomllib.load(handle)
    for assignment in policy["environment"]:
        name, value = assignment.split("=", 1)
        os.environ[name] = value

    entrypoint = policy["entrypoint"]
    assert entrypoint[:2] == ["python3", "-m"] and len(entrypoint) == 3
    launch_module = importlib.import_module(entrypoint[2])
    assert callable(launch_module.run_server)

    profile_id = policy["default_profile"]
    profiles = [profile for profile in policy["profiles"] if profile["id"] == profile_id]
    assert len(profiles) == 1
    selected = profiles[0]
    for model_type in selected["model_types"]:
        importlib.import_module("sglang.srt.models." + model_type)

    argv = expand(policy["arguments"] + selected["arguments"])
    parser = argparse.ArgumentParser(prog="image-self-test")
    ServerArgs.add_cli_args(parser)
    parsed = parser.parse_args(argv)
    assert parsed.model_path == PLACEHOLDERS["{model_snapshot}"]
    assert parsed.served_model_name == PLACEHOLDERS["{served_model}"]
    assert parsed.port == int(PLACEHOLDERS["{port}"])

    major, minor = (int(part) for part in cuda_architecture.split(".", 1))
    assert (major, minor) >= (12, 0) and callable(is_sm120_supported)
    print(json.dumps({
        "cuda_architecture": cuda_architecture,
        "entrypoint": entrypoint,
        "launch_arguments": len(argv),
        "model_modules": len(selected["model_types"]),
    }, sort_keys=True))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
