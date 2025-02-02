# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from pathlib import Path
from re import match
from typing import Callable, Dict, Optional

import numpy as np

from . import Scenario


def duration_to_timedelta(duration: str) -> Optional[np.timedelta64]:
    """Converts a duration like `{time}.{frac}{unit}` to a `np.timedelta64`."""

    frac_to_ns: Dict[str, Callable[[str], str]] = {
        "s": lambda frac: frac.ljust(9, "0")[0:9],
        "ms": lambda frac: frac.ljust(6, "0")[0:6],
        "us": lambda frac: frac.ljust(3, "0")[0:3],
        "ns": lambda frac: "0",  # ns units should not have frac
    }

    p = r"(?P<time>[0-9]+)(\.(?P<frac>[0-9]+))?(?P<unit>s|ms|µs|ns)"
    m = match(p, duration)

    if m is None:
        return None
    else:
        unit = "us" if m.group("unit") == "µs" else m.group("unit")
        time = np.timedelta64(m.group("time"), unit)
        frac = np.timedelta64(frac_to_ns[unit](m.group("frac") or "0"), "ns")
        return time + frac


def results_path(repository: Path, scenario: Scenario, mz_version: str) -> Path:
    mz_version = mz_version.replace(" ", "-")
    mz_version = mz_version.replace("(", "")
    mz_version = mz_version.replace(")", "")
    file = f"optbench-{scenario}-{mz_version}.csv"
    return repository / file
