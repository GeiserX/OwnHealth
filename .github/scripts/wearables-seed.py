"""Prepare six Open Wearables test users and link them through the OwnHealth CLI."""

import json
import os
import subprocess
import time
from datetime import datetime, timedelta, timezone
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import Request, urlopen
from uuid import UUID

OW_API = "http://127.0.0.1:8000/api/v1"
SERIES = {
    "oura": ["resting_heart_rate"],
    "whoop": ["resting_heart_rate"],
    "polar": ["resting_heart_rate"],
    "withings": ["weight"],
    # Open Wearables holds data for these two, so the 0 that OwnHealth stores shows the drop.
    "strava": ["resting_heart_rate"],
    "garmin": ["resting_heart_rate"],
}


def request(path, status=200, *, headers=None, data=None):
    try:
        with urlopen(Request(OW_API + path, data=data, headers=headers or {}), timeout=15) as response:
            if response.status != status:
                raise SystemExit(f"Open Wearables {path} returned {response.status}, expected {status}")
            return json.load(response)
    except HTTPError as error:
        raise SystemExit(f"Open Wearables {path} returned HTTP {error.code}") from None


def post_json(path, status, data, headers):
    return request(
        path, status, headers={**headers, "Content-Type": "application/json"},
        data=json.dumps(data).encode(),
    )


def seed_result(task_id, provider):
    # 0.11.0 returns a Celery task ID. The official worker stores the task result in its Redis backend.
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        raw = subprocess.check_output(
            ["docker", "compose", "exec", "-T", "redis", "redis-cli", "--raw",
             "GET", f"celery-task-meta-{task_id}"],
            text=True, timeout=10,
        ).strip()
        if raw:
            task = json.loads(raw)
            if task["status"] == "SUCCESS":
                return task["result"]
            if task["status"] in ("FAILURE", "REVOKED"):
                raise SystemExit(f"Open Wearables seed for {provider} ended in {task['status']}")
        time.sleep(1)
    raise SystemExit(f"Open Wearables seed for {provider} timed out")


def seed_user(provider, types, developer, api_key):
    seed = 36
    day = (datetime.now(timezone.utc).date() - timedelta(days=2)).isoformat()
    job = post_json("/settings/seed", 202, {
        "num_users": 1,
        "random_seed": seed,
        "profile": {
            "providers": [provider],
            "num_connections": 1,
            "generate_workouts": False,
            "generate_sleep": False,
            "generate_time_series": bool(types),
            "time_series_config": {"enabled_types": types, "date_from": day, "date_to": day},
        },
    }, developer)
    if job["status"] != "dispatched" or job["seed_used"] != seed:
        raise SystemExit(f"Open Wearables did not dispatch the seed for {provider}")
    result = seed_result(UUID(job["task_id"]), provider)
    samples = result["time_series_samples"]
    if result["users"] != 1 or result["connections"] != 1 or (samples > 0) != bool(types):
        raise SystemExit(f"Unexpected Open Wearables seed result for {provider}: {result}")

    users = request(f"/users?{urlencode({'provider': provider, 'search': f'[SEED:{seed}|custom]'})}",
                    headers=api_key)
    if users["total"] != 1:
        raise SystemExit(f"Expected one seeded Open Wearables user for {provider}")
    user = UUID(users["items"][0]["id"])
    connections = request(f"/users/{user}/connections", headers=api_key)
    if [(connection["provider"], connection["status"]) for connection in connections] != [(provider, "active")]:
        raise SystemExit(f"Expected one active Open Wearables connection for {provider}")
    print(f"Seeded {provider} user with {samples} sample{'' if samples == 1 else 's'} in Open Wearables")
    return user


def main():
    email = os.environ["OW_ADMIN_EMAIL"]
    password = os.environ["OW_ADMIN_PASSWORD"]
    login = request(
        "/auth/login", headers={"Content-Type": "application/x-www-form-urlencoded"},
        data=urlencode({"username": email, "password": password}).encode(),
    )
    developer = {"Authorization": f"Bearer {login['access_token']}"}
    key = post_json("/developer/api-keys", 201, {"name": "OwnHealth poller CI"}, developer)["key"]
    api_key = {"X-Open-Wearables-API-Key": key}
    if os.environ.get("OW_SKIP_SEED") == "true":
        # The red control still runs the real poller on a user with no generated data.
        users = [UUID(post_json("/users", 201, {"first_name": "Unseeded control"}, api_key)["id"])]
    else:
        users = [seed_user(provider, types, developer, api_key) for provider, types in SERIES.items()]

    credentials = json.dumps({"admin_password": password, "api_key": key})
    for user in users:
        subprocess.run(
            [os.environ["OWNHEALTH_BIN"], "openwearables-connect", "--profile", "alex",
             "--user", str(user), "--admin-email", email],
            input=credentials, text=True, check=True, timeout=60,
        )


if __name__ == "__main__":
    main()
